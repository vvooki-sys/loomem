//! Request scope resolution shared by E0 and evaluation requests.
use crate::auth::{self, AuthContext};
use crate::handlers::types::SearchRequest;
use crate::handlers::{scope as scope_mod, AppError};
use crate::AppState;
use std::sync::Arc;

pub(super) fn validated_search_streams(
    state: &Arc<AppState>,
    auth: &AuthContext,
    payload: &SearchRequest,
) -> Result<Option<Vec<String>>, AppError> {
    Ok(if let Some(scope) = payload.scope {
        if payload.stream.is_some() || payload.streams.is_some() {
            return Err(AppError::BadRequest(
                "specify scope or stream/streams, not both".into(),
            ));
        }
        let resolution = scope_mod::resolve_scope(scope, auth, &state.store)?;
        Some(resolution.streams.into_iter().map(|(id, _)| id).collect())
    } else {
        // Existing path — byte-identical behaviour for pre-existing callers.
        auth::validate_streams(
            auth,
            payload
                .streams
                .as_deref()
                .or(payload.stream.as_ref().map(std::slice::from_ref)),
        )
        .map_err(|_| AppError::BadRequest("Access denied: cannot search this stream".into()))?
    })
}
