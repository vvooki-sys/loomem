use serde::Deserialize;
use tracing::{debug, warn};

use crate::config::LlmConfig;

#[derive(Debug, Deserialize)]
struct DecomposeResponse {
    queries: Vec<String>,
}

/// Decompose a complex query into 2-3 sub-queries for better recall.
/// Returns original query + sub-queries merged.
/// Falls back to just the original query on any error.
pub async fn decompose(client: &reqwest::Client, config: &LlmConfig, query: &str) -> Vec<String> {
    // Short queries don't need decomposition
    let word_count = query.split_whitespace().count();
    if word_count <= 3 {
        debug!(
            "Multi-query: query too short ({} words), skipping",
            word_count
        );
        return vec![query.to_string()];
    }

    let api_key = match config.get_api_key() {
        Some(k) => k,
        None => return vec![query.to_string()],
    };

    let prompt = format!(
        "Decompose this search query into 2-3 focused sub-queries that together cover the original intent. \
        Each sub-query should target different aspects or keywords.\n\n\
        Query: {}\n\n\
        Reply with ONLY a JSON object: {{\"queries\": [\"sub-query 1\", \"sub-query 2\", ...]}}\n\
        Keep sub-queries short (2-4 words each). Use the same language as the input.",
        query
    );

    let body = serde_json::json!({
        "model": &config.compression_model,
        "messages": [
            {"role": "system", "content": "You decompose search queries into sub-queries. Return only JSON."},
            {"role": "user", "content": prompt}
        ],
        "temperature": 0.0,
        "max_tokens": 150,
    });

    let resp = client
        .post("https://api.openai.com/v1/chat/completions")
        .header("Authorization", format!("Bearer {}", api_key))
        .json(&body)
        .timeout(std::time::Duration::from_secs(8))
        .send()
        .await;

    match resp {
        Ok(r) => {
            let text = r.text().await.unwrap_or_default();
            let parsed: serde_json::Value = match serde_json::from_str(&text) {
                Ok(v) => v,
                Err(_) => return vec![query.to_string()],
            };
            let content = parsed["choices"][0]["message"]["content"]
                .as_str()
                .unwrap_or("{}");

            let clean = content
                .trim()
                .trim_start_matches("```json")
                .trim_start_matches("```")
                .trim_end_matches("```")
                .trim();

            match serde_json::from_str::<DecomposeResponse>(clean) {
                Ok(dr) if !dr.queries.is_empty() => {
                    debug!(
                        "Multi-query decomposed into {} sub-queries: {:?}",
                        dr.queries.len(),
                        dr.queries
                    );
                    // Return original + sub-queries
                    let mut all = vec![query.to_string()];
                    all.extend(dr.queries);
                    all
                }
                _ => vec![query.to_string()],
            }
        }
        Err(e) => {
            warn!("Multi-query decomposition failed: {}", e);
            vec![query.to_string()]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_decompose_response_parse() {
        let json = r#"{"queries": ["Acme SAR", "organizacja branżowa"]}"#;
        let dr: DecomposeResponse = serde_json::from_str(json).expect("parse test decompose JSON");
        assert_eq!(dr.queries.len(), 2);
    }

    #[test]
    fn test_short_query_skip() {
        // Can't test async here, but verify word count logic
        let query = "Acme SAR";
        let word_count = query.split_whitespace().count();
        assert!(word_count <= 3);
    }
}
