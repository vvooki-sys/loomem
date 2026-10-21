//! Rebuild vectors without publishing inference results after a chunk changes.
use crate::AppState;
use loomem_core::storage::{Chunk, EmbeddingTarget, RocksDbStore};
use std::sync::Arc;

#[derive(Default)]
struct Counts {
    embedded: usize,
    stale: usize,
    failed: usize,
}

pub(super) async fn run(state: Arc<AppState>) {
    let chunks = match state.store.get_all_chunks() {
        Ok(chunks) => chunks,
        Err(error) => {
            tracing::error!(%error, "Re-embed-all: failed to enumerate chunks");
            return;
        }
    };
    let existing = match state.store.get_all_embeddings() {
        Ok(existing) => existing,
        Err(error) => {
            tracing::error!(%error, "Re-embed-all: failed to enumerate embeddings");
            return;
        }
    };
    let mut cleared = 0_usize;
    for (id, _) in existing {
        match state.store.delete_embedding(&id) {
            Ok(()) => cleared += 1,
            Err(error) => tracing::warn!(%error, "Re-embed-all: failed to clear vector"),
        }
    }
    let mut counts = Counts::default();
    for batch in chunks.chunks(50) {
        embed_batch(&state, batch, &mut counts).await;
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    }
    tracing::info!(
        embedded = counts.embedded,
        stale = counts.stale,
        failed = counts.failed,
        total = chunks.len(),
        cleared,
        "Re-embed-all complete"
    );
}

fn capture_targets(
    store: &RocksDbStore,
    batch: &[Chunk],
    counts: &mut Counts,
) -> Vec<(EmbeddingTarget, String)> {
    let mut targets = Vec::new();
    for chunk in batch {
        match store.embedding_target(&chunk.id, &chunk.content) {
            Ok(Some(target)) => targets.push((target, chunk.content.clone())),
            Ok(None) => counts.stale += 1,
            Err(error) => {
                counts.failed += 1;
                tracing::warn!(%error, "Re-embed-all: failed to capture target");
            }
        }
    }
    targets
}

async fn embed_batch(state: &Arc<AppState>, batch: &[Chunk], counts: &mut Counts) {
    let targets = capture_targets(&state.store, batch, counts);
    if targets.is_empty() {
        return;
    }
    let texts: Vec<String> = targets.iter().map(|(_, content)| content.clone()).collect();
    let result = if let Some(ref embedder) = state.local_embedder {
        embedder.embed_batch(&texts)
    } else if state.config.llm.get_api_key().is_some() {
        loomem_core::llm::embed_batch(&state.http_client, &state.config.llm, &texts).await
    } else {
        Err(anyhow::anyhow!("no embedding provider"))
    };
    match result {
        Ok(vectors) => publish(&state.store, targets, vectors, counts),
        Err(error) => {
            counts.failed += targets.len();
            tracing::warn!(%error, "Re-embed-all: batch inference failed");
        }
    }
}

fn publish(
    store: &RocksDbStore,
    targets: Vec<(EmbeddingTarget, String)>,
    vectors: Vec<Vec<f32>>,
    counts: &mut Counts,
) {
    // Do not zip a partial provider response: that misattributes missing rows
    // and can publish vectors against the wrong request positions.
    if targets.len() != vectors.len() {
        counts.failed += targets.len();
        tracing::warn!(
            expected = targets.len(),
            received = vectors.len(),
            "Re-embed-all: provider cardinality mismatch"
        );
        return;
    }
    for ((target, _), vector) in targets.into_iter().zip(vectors) {
        match store.store_embedding_if_current(&target, vector) {
            Ok(true) => counts.embedded += 1,
            Ok(false) => counts.stale += 1,
            Err(error) => {
                counts.failed += 1;
                tracing::warn!(%error, "Re-embed-all: failed to publish vector");
            }
        }
    }
}

#[cfg(test)]
#[path = "reembed_tests.rs"]
mod tests;
