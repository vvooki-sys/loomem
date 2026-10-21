//! All lineage tombstones precede independent, retryable derived-index cleanup.
use super::DeleteOutcome;
use anyhow::{bail, Result};
use loomem_core::{graph::GraphStore, storage::RocksDbStore, tantivy_index::TantivyIndex};
use tokio::sync::Mutex;

pub(super) async fn cleanup(
    store: &RocksDbStore,
    tantivy: &Mutex<TantivyIndex>,
    graph: &GraphStore,
    ids: Vec<String>,
) -> DeleteOutcome {
    let mut text_errors = Vec::new();
    let mut embedding_errors = Vec::new();
    let mut graph_errors = Vec::new();
    for id in &ids {
        let text = tantivy.lock().await.delete_document(id);
        collect_error(&mut text_errors, id, text);
        collect_error(&mut embedding_errors, id, store.delete_embedding(id));
        collect_error(&mut graph_errors, id, graph.remove_chunk_references(id));
    }
    DeleteOutcome {
        store_deleted: true,
        store: Ok(()),
        tantivy: aggregate("tantivy", ids.len(), text_errors),
        embedding: aggregate("embedding", ids.len(), embedding_errors),
        graph: aggregate("graph", ids.len(), graph_errors),
    }
}

fn collect_error(errors: &mut Vec<String>, id: &str, result: Result<()>) {
    if let Err(error) = result {
        tracing::error!(%id, %error, "ingest lineage cleanup failed; retry required");
        errors.push(format!("{id}: {error}"));
    }
}

fn aggregate(surface: &str, total: usize, errors: Vec<String>) -> Result<()> {
    if !errors.is_empty() {
        bail!(
            "{surface} cleanup failed for {}/{} lineage chunks: {}",
            errors.len(),
            total,
            errors.join("; ")
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests;
