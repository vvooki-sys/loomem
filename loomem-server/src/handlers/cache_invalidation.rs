//! Shared invalidation for mutation handlers: search and ambient observe writes.
use crate::AppState;

pub(super) async fn clear(state: &AppState) {
    state.ambient_cache.clear();
    state.query_cache.lock().await.clear();
}
