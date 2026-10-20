//! Startup index reconciliation and offline embedding maintenance.
use anyhow::{Context, Result};
use loomem_core::{
    config::TantivyConfig, local_embeddings::LocalEmbedder, Config, RocksDbStore, TantivyIndex,
};
use std::future::Future;
use std::sync::Arc;
use tracing::{info, warn};

pub(crate) fn check_index_drift(
    config: &TantivyConfig,
    store: &RocksDbStore,
    tantivy: &mut TantivyIndex,
) -> Result<()> {
    store
        .cleanup_legacy_entity_markers()
        .context("Failed to remove obsolete extraction payloads before serving")?;
    tantivy
        .repair_ingest_event_dates(store)
        .context("Failed to repair versioned event-date index before serving")?;
    let tantivy_count = tantivy
        .count()
        .context("Failed to count Tantivy documents")?;
    let chunk_count = u64::try_from(
        store
            .get_all_chunks()
            .context("Failed to read RocksDB chunks for drift check")?
            .len(),
    )
    .context("Chunk count exceeds u64")?;
    let drift = index_drift_pct(chunk_count, tantivy_count);
    let threshold = config.drift_warn_pct;
    if drift > threshold {
        warn!("Tantivy drift detected: tantivy_docs={tantivy_count}, rocksdb_chunks={chunk_count}, drift={drift:.2}% (threshold={threshold:.2}%). Run POST /v1/rebuild-tantivy to fix.");
        if config.auto_rebuild_on_drift {
            tantivy
                .rebuild_from_rocksdb(store)
                .context("Failed to auto-rebuild Tantivy on drift")?;
            info!("Tantivy auto-rebuild complete");
        }
    } else {
        info!(
            "Tantivy sync OK: {tantivy_count} docs ({drift:.2}% drift, threshold={threshold:.2}%)"
        );
    }
    Ok(())
}

fn index_drift_pct(primary: u64, indexed: u64) -> f64 {
    if primary == 0 {
        return if indexed == 0 { 0.0 } else { 100.0 };
    }
    // truncation intentional: approximate drift ratios tolerate f64 count rounding above 2^53.
    (primary.abs_diff(indexed) as f64 / primary as f64) * 100.0
}

enum Backend {
    Local(Arc<LocalEmbedder>),
    Openai {
        client: reqwest::Client,
        api_key: String,
        model: String,
    },
}

fn reembed_backend(config: &Config) -> Result<Backend> {
    match config.llm.embedding_provider.as_str() {
        "local" => {
            let model_dir = config.llm.embedding_model_path.clone().unwrap_or_else(|| {
                let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
                format!("{home}/.loomem/models/multilingual-e5-small")
            });
            let embedder =
                loomem_core::local_embeddings::try_load(&model_dir, config.llm.embedding_dim)
                    .context("failed to load local embedding model for re-embedding")?;
            Ok(Backend::Local(Arc::new(embedder)))
        }
        "openai" => Ok(Backend::Openai {
            client: config
                .llm
                .build_http_client()
                .context("build HTTP client")?,
            api_key: config
                .llm
                .get_api_key()
                .context("OPENAI_API_KEY required to re-embed with the openai provider")?,
            model: config.llm.embedding_model.clone(),
        }),
        other => anyhow::bail!("unknown embedding_provider {other:?}"),
    }
}

async fn embed(backend: &Backend, content: String) -> Result<Vec<f32>> {
    match backend {
        Backend::Local(e) => e.embed(&content),
        Backend::Openai {
            client,
            api_key,
            model,
        } => loomem_core::embeddings::embed(client, api_key, model, &content).await,
    }
}

// Capture the lifecycle token before any inference; publication rechecks it atomically.
async fn reembed_chunk<F, Fut>(store: &RocksDbStore, id: &str, infer: F) -> Result<bool>
where
    F: FnOnce(String) -> Fut,
    Fut: Future<Output = Result<Vec<f32>>>,
{
    let Some(chunk) = store
        .get_chunk(id)
        .with_context(|| format!("read chunk {id}"))?
    else {
        return Ok(false);
    };
    let Some(target) = store
        .embedding_target(id, &chunk.content)
        .context("capture embedding target")?
    else {
        return Ok(false);
    };
    let vector = infer(chunk.content).await.context("re-embed chunk")?;
    store
        .store_embedding_if_current(&target, vector)
        .with_context(|| format!("store embedding {id}"))
}

// Do not begin a dimension migration that cannot update retained tombstone/orphan
// vectors: removing recoverable data needs an explicit lifecycle operation.
fn validate_reembed_sources(
    store: &RocksDbStore,
    existing: &[(String, Vec<f32>)],
    target_dim: usize,
) -> Result<()> {
    for (id, vector) in existing {
        if vector.len() == target_dim {
            continue;
        }
        let chunk = store.get_chunk(id).context("inspect re-embedding source")?;
        anyhow::ensure!(
            chunk.is_some_and(|chunk| chunk.deleted_at.is_none()),
            "Cannot migrate embedding dimension: retained deleted or missing source {id} has dimension {}; explicitly restore or purge it before retrying",
            vector.len()
        );
    }
    Ok(())
}

fn record_reembedded_dimension(store: &RocksDbStore, target_dim: usize) -> Result<()> {
    for (id, vector) in store
        .get_all_embeddings()
        .context("verify migrated embeddings")?
    {
        anyhow::ensure!(
            vector.len() == target_dim,
            "Re-embedding incomplete: {id} still has dimension {}, expected {target_dim}; dimension metadata unchanged",
            vector.len()
        );
    }
    store
        .set_embedding_dim(target_dim)
        .context("record embedding_dim")
}

/// Offline re-embedding with the server stopped; starts no listener or workers.
pub(crate) async fn run_reembed(config: &Config, store: &RocksDbStore) -> Result<()> {
    let dim = config.llm.embedding_dim;
    let backend = reembed_backend(config)?;
    let existing = store.get_all_embeddings().context("list embeddings")?;
    validate_reembed_sources(store, &existing, dim)?;
    let total = existing.len();
    info!("Re-embedding {total} vectors, target dim={dim}");
    let mut done = 0usize;
    let mut skipped = 0usize;
    for (id, _old) in existing {
        if reembed_chunk(store, &id, |content| embed(&backend, content)).await? {
            done += 1;
            if done.is_multiple_of(200) {
                info!("  re-embedded {done}/{total}");
            }
        } else {
            skipped += 1;
        }
    }
    record_reembedded_dimension(store, dim)?;
    info!("Re-embedding complete: {done} re-embedded, {skipped} skipped (missing, deleted or changed), dim recorded = {dim}");
    Ok(())
}

#[cfg(test)]
mod tests;
