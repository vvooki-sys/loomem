//! Start background extraction with the same configured privacy filter as ingest.
use loomem_core::entity_extraction_queue::{
    spawn_worker, EntityExtractionQueue, EntityExtractionWorkerContext,
};
use loomem_core::graph::GraphStore;
use loomem_core::{Config, CostTracker, PiiFilter, RocksDbStore, TantivyIndex};
use std::sync::Arc;

pub(crate) fn start(
    config: &Config,
    store: Arc<RocksDbStore>,
    graph: Arc<GraphStore>,
    tantivy: Arc<tokio::sync::Mutex<TantivyIndex>>,
    client: reqwest::Client,
    pii: Arc<PiiFilter>,
) -> Option<EntityExtractionQueue> {
    if !config.entity_extraction.enabled {
        return None;
    }
    if config.llm.get_api_key().is_none() {
        tracing::warn!("Entity extraction disabled (no API key)");
        return None;
    }
    let context = EntityExtractionWorkerContext {
        llm_config: config.llm.clone(),
        cost_tracker: Arc::new(CostTracker::new(
            store.clone(),
            config.cost.clone(),
            client.clone(),
        )),
        config: config.entity_extraction.clone(),
        pii_filter: pii,
    };
    tracing::info!("Entity extraction queue started (LLM NER enabled)");
    Some(spawn_worker(store, graph, tantivy, client, context))
}
