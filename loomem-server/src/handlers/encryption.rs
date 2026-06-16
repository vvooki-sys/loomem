//! Admin handler for encryption-state observability (cycle /144).
//!
//! - `GET /v1/encryption/status` — admin-gated, non-secret snapshot of the
//!   encryption-at-rest provider: `{enabled, provider, master_key_version,
//!   master_key_fingerprint, dek_count}`. Returns no raw key or DEK material.
//!   ADR-013 § Decision 8.

use std::sync::Arc;

use axum::{extract::State, Json};
use loomem_core::EncryptionStatus;

use super::AppError;
use crate::AppState;

/// `GET /v1/encryption/status` — admin-only. Defence-in-depth: even though the
/// non-admin REST middleware already blocks this path, the handler re-checks
/// `is_admin` (→ 403) like the other in-handler-gated admin endpoints.
pub async fn encryption_status_handler(
    State(state): State<Arc<AppState>>,
    request: axum::extract::Request,
) -> Result<Json<EncryptionStatus>, AppError> {
    super::bench::require_admin_forbidden(&request)?;
    Ok(Json(state.store.encryption_provider().status()))
}
