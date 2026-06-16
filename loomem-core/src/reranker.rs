use anyhow::Result;
use serde::Deserialize;
use tracing::{debug, warn};

use crate::config::LlmConfig;

#[derive(Debug, Deserialize)]
struct RerankResponse {
    indices: Vec<usize>,
}

/// Rerank search results using LLM. Returns reordered indices (best first).
/// Falls back to original order on any error.
pub async fn rerank(
    client: &reqwest::Client,
    config: &LlmConfig,
    query: &str,
    chunks: &[String], // content of each result
    top_k: usize,
) -> Result<Vec<usize>> {
    let api_key = match config.get_api_key() {
        Some(k) => k,
        None => {
            debug!("Reranker: no API key, returning original order");
            return Ok((0..chunks.len().min(top_k)).collect());
        }
    };

    if chunks.is_empty() {
        return Ok(vec![]);
    }

    // Build numbered chunk list (truncate each to ~200 chars for cost efficiency)
    let numbered: Vec<String> = chunks
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let truncated: String = c.chars().take(200).collect();
            format!("[{}] {}", i, truncated)
        })
        .collect();

    let prompt = format!(
        "Given the query and {} search results below, return the indices of the {} most relevant results, ordered by relevance (best first).\n\n\
        Query: {}\n\n\
        Results:\n{}\n\n\
        Reply with ONLY a JSON object: {{\"indices\": [0, 3, 1, ...]}}",
        chunks.len(),
        top_k.min(chunks.len()),
        query,
        numbered.join("\n\n")
    );

    let body = serde_json::json!({
        "model": &config.compression_model, // gpt-4.1-mini
        "messages": [
            {"role": "system", "content": "You are a search result reranker. Return only JSON."},
            {"role": "user", "content": prompt}
        ],
        "temperature": 0.0,
        "max_tokens": 100,
    });

    let resp = client
        .post("https://api.openai.com/v1/chat/completions")
        .header("Authorization", format!("Bearer {}", api_key))
        .json(&body)
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await;

    match resp {
        Ok(r) => {
            let status = r.status();
            let text = r.text().await.unwrap_or_default();
            if !status.is_success() {
                warn!(
                    "Reranker API error {}: {}",
                    status,
                    &text[..200.min(text.len())]
                );
                return Ok((0..chunks.len().min(top_k)).collect());
            }

            // Parse OpenAI response → extract content
            let parsed: serde_json::Value = serde_json::from_str(&text)?;
            let content = parsed["choices"][0]["message"]["content"]
                .as_str()
                .unwrap_or("{}");

            // Parse the JSON indices from content (handle markdown code blocks)
            let clean = content
                .trim()
                .trim_start_matches("```json")
                .trim_start_matches("```")
                .trim_end_matches("```")
                .trim();

            match serde_json::from_str::<RerankResponse>(clean) {
                Ok(rr) => {
                    // Validate indices
                    let valid: Vec<usize> = rr
                        .indices
                        .into_iter()
                        .filter(|&i| i < chunks.len())
                        .take(top_k)
                        .collect();
                    debug!("Reranker returned {} valid indices", valid.len());
                    Ok(valid)
                }
                Err(e) => {
                    warn!("Reranker parse error: {} — content: {}", e, clean);
                    Ok((0..chunks.len().min(top_k)).collect())
                }
            }
        }
        Err(e) => {
            warn!("Reranker request failed: {}", e);
            Ok((0..chunks.len().min(top_k)).collect())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rerank_response_parse() {
        let json = r#"{"indices": [2, 0, 4, 1, 3]}"#;
        let rr: RerankResponse = serde_json::from_str(json).expect("parse test rerank JSON");
        assert_eq!(rr.indices, vec![2, 0, 4, 1, 3]);
    }

    #[test]
    fn test_rerank_response_parse_with_markdown() {
        let content = "```json\n{\"indices\": [1, 0]}\n```";
        let clean = content
            .trim()
            .trim_start_matches("```json")
            .trim_start_matches("```")
            .trim_end_matches("```")
            .trim();
        let rr: RerankResponse = serde_json::from_str(clean).expect("parse cleaned rerank JSON");
        assert_eq!(rr.indices, vec![1, 0]);
    }
}
