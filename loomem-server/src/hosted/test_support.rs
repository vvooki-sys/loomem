//! Fixtures for the Hosted adapter tests: a real engine over temporary
//! storage, without the local embedding model or any LLM-backed subsystem.
use super::SharedEngine;
use loomem_core::Config;
use std::path::Path;

fn config(root: &Path) -> Config {
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../config.toml");
    let path = root.join("config.toml");
    std::fs::copy(&source, &path).expect("copy reference config");
    let entities = root.join("entities.toml");
    std::fs::write(&entities, "").expect("write empty entities file");
    let mut config = Config::load(&path).expect("load reference config");
    config.llm.embedding_provider = "openai".into();
    config.storage.vector_enabled = false;
    config.search.entities_file = entities.to_string_lossy().into_owned();
    config.search.synonyms_file = root.join("synonyms.toml").to_string_lossy().into_owned();
    config.search.rerank_enabled = false;
    config.scheduler.enabled = false;
    config.entity_extraction.enabled = false;
    config.knowledge_extraction.enabled = false;
    config.dream.enabled = false;
    config.pii.blocklist_file = root.join("blocklist.txt").to_string_lossy().into_owned();
    config
}

pub(super) async fn new_engine() -> (tempfile::TempDir, SharedEngine) {
    let root = tempfile::tempdir().expect("temporary engine root");
    let engine = SharedEngine::new(&root.path().join("engine"), config(root.path()))
        .expect("open shared engine");
    (root, engine)
}
