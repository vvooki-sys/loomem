use anyhow::{Context, Result};
use loomem_core::{config::LlmConfig, local_embeddings::LocalEmbedder};
use std::sync::Arc;

/// A configured local provider must never silently become a cloud provider.
pub(crate) fn load(config: &LlmConfig) -> Result<Option<Arc<LocalEmbedder>>> {
    if config.embedding_provider != "local" {
        return Ok(None);
    }
    let model_dir = config.embedding_model_path.clone().unwrap_or_else(|| {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        format!("{home}/.loomem/models/multilingual-e5-small")
    });
    let embedder = loomem_core::local_embeddings::try_load(&model_dir, config.embedding_dim)
        .context("Local embedding provider could not load its model; refusing cloud fallback. Check embedding_model_path and the local-embeddings feature")?;
    Ok(Some(Arc::new(embedder)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_local_model_fails_even_with_a_cloud_key() {
        let dir = tempfile::tempdir().unwrap();
        let config = LlmConfig {
            embedding_provider: "local".into(),
            embedding_model_path: Some(dir.path().to_string_lossy().into_owned()),
            api_key: Some("synthetic-test-key".into()),
            ..Default::default()
        };
        let error = load(&config)
            .err()
            .expect("local provider must fail closed");
        assert!(error.to_string().contains("refusing cloud fallback"));
    }

    #[test]
    fn explicit_cloud_provider_does_not_load_local_artifacts() {
        assert!(load(&LlmConfig::default()).unwrap().is_none());
    }
}
