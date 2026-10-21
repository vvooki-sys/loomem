//! Embedding backfill publishes only against the lifecycle captured before inference.
use std::collections::HashSet;

use axum::{http::StatusCode, Json};
use loomem_core::{embeddings, storage::Chunk};

use crate::AppState;

pub(super) async fn run(state: &AppState) -> Result<Json<serde_json::Value>, StatusCode> {
    if !state.config.storage.vector_enabled {
        return Ok(Json(serde_json::json!({
            "status": "skipped", "reason": "vector_enabled=false"
        })));
    }
    // Preserve the existing local-first provider policy and fail-fast behavior.
    if state.local_embedder.is_none() && state.config.llm.get_api_key().is_none() {
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    }
    let missing = missing_chunks(state).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let total_missing = missing.len();
    tracing::info!("embed-missing: {} chunks need embeddings", total_missing);
    let mut embedded = 0usize;
    let mut failed = 0usize;
    let mut skipped = 0usize;
    for chunk in missing {
        match embed_current(state, &chunk).await {
            Ok(true) => embedded += 1,
            Ok(false) => skipped += 1,
            Err(e) => {
                tracing::warn!("embed-missing: failed for {}: {}", chunk.id, e);
                failed += 1;
                tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
            }
        }
        if embedded.is_multiple_of(50) && embedded > 0 {
            tracing::info!("embed-missing: progress {}/{}", embedded, total_missing);
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
    }
    tracing::info!(
        "embed-missing complete: {embedded} embedded, {failed} failed, {skipped} skipped"
    );
    Ok(Json(serde_json::json!({
        "status": "done", "total_missing": total_missing,
        "embedded": embedded, "failed": failed, "skipped": skipped,
    })))
}

fn missing_chunks(state: &AppState) -> anyhow::Result<Vec<Chunk>> {
    let chunks = state.store.get_all_chunks()?;
    let embedded_ids: HashSet<_> = state
        .store
        .get_all_embeddings()?
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    Ok(chunks
        .into_iter()
        .filter(|c| !embedded_ids.contains(&c.id))
        .collect())
}

/// False means the source was stale/deleted before or during inference; this is
/// a skipped publication, never a successful embedding or provider failure.
async fn embed_current(state: &AppState, chunk: &Chunk) -> anyhow::Result<bool> {
    let Some(target) = state.store.embedding_target(&chunk.id, &chunk.content)? else {
        return Ok(false);
    };
    let embedding = infer(state, &chunk.content).await?;
    state.store.store_embedding_if_current(&target, embedding)
}

async fn infer(state: &AppState, content: &str) -> anyhow::Result<Vec<f32>> {
    if let Some(embedder) = &state.local_embedder {
        return embedder.embed(content);
    }
    let api_key = state
        .config
        .llm
        .get_api_key()
        .ok_or_else(|| anyhow::anyhow!("No embedding provider"))?;
    embeddings::embed(
        &state.http_client,
        &api_key,
        &state.config.llm.embedding_model,
        content,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stale_deleted_and_absent_sources_skip_before_inference() {
        let (_router, state) = crate::tests::make_test_app();
        let chunk: Chunk = serde_json::from_value(serde_json::json!({
            "id": "backfill-stale", "content": "synthetic fact", "stream": "test",
            "level": 0, "score": 1.0, "timestamp": 1, "consolidated": false,
            "dormant": false, "in_progress": false
        }))
        .unwrap();
        assert!(!embed_current(&state, &chunk).await.unwrap());
        let mut updated = chunk.clone();
        updated.content = "updated fact".into();
        state.store.store_chunk(&updated).unwrap();
        assert!(!embed_current(&state, &chunk).await.unwrap());
        state.store.delete_by_id(&updated.id).unwrap();
        assert!(!embed_current(&state, &updated).await.unwrap());
        assert!(state.store.get_embedding(&chunk.id).unwrap().is_none());
    }
}
