use anyhow::Result;
use serde::Deserialize;
use tracing::{debug, warn};

use crate::config::LlmConfig;

#[derive(Debug, Deserialize)]
struct RerankResponse {
    indices: Vec<usize>,
}

/// Number each document for the prompt (`[i] text`), keeping at most
/// `max_chars` characters (not bytes) of each. `max_chars = 200` is the
/// previous hardcoded cap, so the prompt stays byte-identical.
pub fn numbered_documents(chunks: &[String], max_chars: usize) -> Vec<String> {
    chunks
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let truncated: String = c.chars().take(max_chars).collect();
            format!("[{}] {}", i, truncated)
        })
        .collect()
}

/// Rerank search results using LLM. Returns reordered indices (best first).
/// Falls back to original order on any error. Each document is cut to
/// `max_chars` characters (`[search] rerank_llm_max_chars`).
pub async fn rerank(
    client: &reqwest::Client,
    config: &LlmConfig,
    query: &str,
    chunks: &[String], // content of each result
    top_k: usize,
    max_chars: usize,
) -> Result<Vec<usize>> {
    let provider = match config.active_provider() {
        Ok(p) => p,
        Err(e) => {
            debug!("Reranker: {e}, returning original order");
            return Ok((0..chunks.len().min(top_k)).collect());
        }
    };

    if chunks.is_empty() {
        return Ok(vec![]);
    }

    // Build numbered chunk list (truncate each to `max_chars` for cost efficiency)
    let numbered = numbered_documents(chunks, max_chars);

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

    let body = provider.chat_body(serde_json::json!({
        "model": &config.compression_model,
        "messages": [
            {"role": "system", "content": "You are a search result reranker. Return only JSON."},
            {"role": "user", "content": prompt}
        ],
        "temperature": 0.0,
        "max_tokens": 100,
    }));

    let request = provider
        .chat_request(client)
        .json(&body)
        .timeout(std::time::Duration::from_secs(10));
    let resp = provider
        .send_chat(request, "reranker", &config.compression_model)
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
            crate::llm_provider::ensure_choice_not_truncated(
                &parsed,
                &config.compression_model,
                crate::llm_provider::completion_limit_of(&body),
            )?;
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

    /// The pre-flag prompt body, assembled exactly as the old inline code did.
    fn legacy_numbered(chunks: &[String]) -> Vec<String> {
        let mut out = Vec::new();
        for (i, c) in chunks.iter().enumerate() {
            let truncated: String = c.chars().take(200).collect();
            out.push(format!("[{}] {}", i, truncated));
        }
        out
    }

    fn docs() -> Vec<String> {
        vec![
            "short".to_string(),
            "x".repeat(450),
            "zażółć gęślą jaźń ".repeat(20),
            String::new(),
        ]
    }

    #[test]
    fn numbered_documents_at_200_matches_the_legacy_prompt_bytes() {
        let docs = docs();
        let new = numbered_documents(&docs, 200);
        assert_eq!(new, legacy_numbered(&docs));
        assert_eq!(new.join("\n\n"), legacy_numbered(&docs).join("\n\n"));
        assert_eq!(new[1], format!("[1] {}", "x".repeat(200)));
        assert_eq!(new[3], "[3] ");
    }

    #[test]
    fn numbered_documents_with_a_smaller_cap_is_shorter() {
        let docs = docs();
        let new = numbered_documents(&docs, 50);
        assert_eq!(new[0], "[0] short");
        assert_eq!(new[1], format!("[1] {}", "x".repeat(50)));
        for (short, long) in new.iter().zip(numbered_documents(&docs, 200)) {
            assert!(short.len() <= long.len());
        }
        assert!(new[2].len() < numbered_documents(&docs, 200)[2].len());
    }

    #[test]
    fn numbered_documents_counts_characters_not_bytes() {
        let docs = vec!["żółć".repeat(10)];
        let new = numbered_documents(&docs, 3);
        // Three two-byte characters; a byte cut would split a char or panic.
        assert_eq!(new, vec!["[0] żół".to_string()]);
        assert_eq!(new[0].chars().count(), "[0] ".len() + 3);
        assert!(numbered_documents(&[], 200).is_empty());
    }

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
