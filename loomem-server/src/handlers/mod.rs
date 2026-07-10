pub mod admin;
pub mod ambient;
pub mod backfill_encrypt;
pub mod bench;
pub mod co_occur;
pub mod context;
pub mod dashboard;
pub mod date_filter;
pub mod delete;
pub mod encryption;
pub mod feedback;
pub mod graph_repair;
pub mod ingest;
pub mod purge;
pub mod scope;
pub mod search;
pub mod stats;
pub mod stream_stats;
pub mod types;
pub mod workers;

// Re-export all public handler functions so main.rs doesn't need to change
pub use admin::{
    admin_ui_handler, api_delete_memory_handler, api_purge_namespace_handler,
    api_update_memory_handler, backfill_content_type_handler, backfill_event_dates_handler,
    boost_handler, build_graph_handler, delete_handler, dream_handler, extract_entities_handler,
    generate_memory_md_handler, graph_entity_handler, graph_stats_handler, health_handler,
    index_sync_health_handler, namespaces_handler, purge_namespace_handler, re_embed_all_handler,
    rebuild_tantivy_handler, rekey_name_index_handler, reprocess_legacy_handler,
    reset_backfill_handler, reset_importance_handler, status_handler, tag_tier_handler,
    whoami_handler,
};
pub use ambient::ambient_handler;
use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
pub use backfill_encrypt::{
    encrypt_at_rest_backfill_handler, encrypt_at_rest_backfill_status_handler,
};
pub use bench::{admin_bench_history_handler, admin_bench_run_handler};
pub use co_occur::co_occur_handler;
pub use context::context_pack_handler;
pub use dashboard::{dashboard_memory_handler, memory_chain_handler};
pub use encryption::encryption_status_handler;
pub use feedback::feedback_handler;
pub use graph_repair::graph_entity_stream_repair_handler;
pub use ingest::{embed_missing_handler, retag_all_handler, score_all_handler, store_handler};
pub use search::{associate_handler, search_handler};
use serde_json::json;
pub use stats::{
    advisory_adjust_weights_handler, advisory_effectiveness_handler, advisory_handler,
    advisory_outcome_handler, assoc_consumed_handler, dream_discoveries_handler,
    dream_trigger_handler, stats_feedback_handler, stats_profile_handler, stats_stream_handler,
    stats_summary_handler, stats_trends_handler,
};
pub use stream_stats::{admin_stream_stats_handler, user_stream_stats_handler};
pub use workers::{
    admin_streams_stats_handler, admin_workers_pause_handler, admin_workers_pause_one_handler,
    admin_workers_resume_handler, admin_workers_resume_one_handler, admin_workers_status_handler,
};

/// Shared error type for all handlers.
#[derive(Debug)]
pub enum AppError {
    Internal(anyhow::Error),
    BadRequest(String),
    Forbidden(String),
    NotFound(String),
    /// HTTP 413 Payload Too Large (e.g. feedback batch exceeds size cap).
    PayloadTooLarge(String),
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        match self {
            AppError::Internal(err) => {
                let message = format!("{:#}", err);
                tracing::error!("Request error: {}", message);

                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({
                        "error": message
                    })),
                )
                    .into_response()
            }
            AppError::BadRequest(message) => (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": message
                })),
            )
                .into_response(),
            AppError::Forbidden(message) => (
                StatusCode::FORBIDDEN,
                Json(json!({
                    "error": message
                })),
            )
                .into_response(),
            AppError::NotFound(message) => (
                StatusCode::NOT_FOUND,
                Json(json!({
                    "error": message
                })),
            )
                .into_response(),
            AppError::PayloadTooLarge(message) => (
                StatusCode::PAYLOAD_TOO_LARGE,
                Json(json!({
                    "error": message
                })),
            )
                .into_response(),
        }
    }
}

impl<E> From<E> for AppError
where
    E: Into<anyhow::Error>,
{
    fn from(err: E) -> Self {
        Self::Internal(err.into())
    }
}
