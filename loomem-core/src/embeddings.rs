use anyhow::Result;

use crate::config::LlmConfig;
use crate::llm;

/// Call OpenAI embedding API with retry logic (wrapper for llm::embed)
pub async fn embed(
    client: &reqwest::Client,
    api_key: &str,
    model: &str,
    text: &str,
) -> Result<Vec<f32>> {
    // Create a temporary config for compatibility
    let config = LlmConfig {
        provider: "openai".to_string(),
        embedding_provider: "openai".to_string(),
        api_key: Some(api_key.to_string()),
        api_key_env: "OPENAI_API_KEY".to_string(),
        embedding_model: model.to_string(),
        embedding_model_path: None,
        embedding_dim: 1536,
        compression_model: "gpt-4o-mini".to_string(),
        timeout_secs: 30,
        fallback_to_regex: true,
    };

    llm::embed(client, &config, text).await
}
