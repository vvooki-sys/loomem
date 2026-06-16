//! LLM-based Named Entity Recognition.
//!
//! Sends chunk content to OpenAI and extracts entities + relations
//! with confidence scores. Complements dictionary-based extraction.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

/// (chunk_id, content, dict_entities as [(name, type)]) batch entry.
pub type ChunkBatchEntry = (String, String, Vec<(String, String)>);

const SYSTEM_PROMPT: &str = r#"Extract named entities and relations from the text chunks below. Each chunk is delimited by [CHUNK id="..."] ... [/CHUNK].

For each chunk, identify:
- Entities: persons, organizations, projects, technologies, places
- Relations between entities: subject-relation-object triples

Rules:
1. Preserve original language and diacritics (e.g., "Jan", not "Rafal")
2. Use canonical full names when inferable from context
3. Confidence: 0.9+ for explicitly named, 0.5-0.8 for inferred from context
4. Entity types: Person, Organization, Project, Technology, Place
5. Relations: works_at, member_of, uses, manages, created, located_in, related_to
6. Skip entities already listed in known_entities for each chunk
7. Return ONLY new discoveries

Return JSON (no markdown, no code blocks):
{"chunks":[{"chunk_id":"...","entities":[{"name":"...","entity_type":"...","confidence":0.9,"aliases":[]}],"relations":[{"subject":"...","relation":"...","object":"...","confidence":0.8}]}]}"#;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmEntity {
    pub name: String,
    pub entity_type: String,
    pub confidence: f64,
    #[serde(default)]
    pub aliases: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmRelation {
    pub subject: String,
    pub relation: String,
    pub object: String,
    pub confidence: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkExtraction {
    pub chunk_id: String,
    #[serde(default)]
    pub entities: Vec<LlmEntity>,
    #[serde(default)]
    pub relations: Vec<LlmRelation>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LlmNerResponse {
    chunks: Vec<ChunkExtraction>,
}

/// Build the user message for a batch of chunks.
pub fn build_prompt(chunks: &[ChunkBatchEntry]) -> String {
    // chunks: Vec<(chunk_id, content, dict_entities as [(name, type)])>
    let mut parts = Vec::new();
    for (id, content, dict_ents) in chunks {
        let known = if dict_ents.is_empty() {
            String::new()
        } else {
            dict_ents
                .iter()
                .map(|(n, _)| n.as_str())
                .collect::<Vec<_>>()
                .join(",")
        };
        parts.push(format!(
            "[CHUNK id=\"{}\" known_entities=\"{}\"]\n{}\n[/CHUNK]",
            id, known, content
        ));
    }
    parts.join("\n\n")
}

/// Estimate token count for a string (~4 chars per token for mixed lang).
pub fn estimate_tokens(text: &str) -> usize {
    text.len() / 4 + 1
}

/// Call OpenAI to extract entities from a batch of chunks.
pub async fn extract_entities_llm(
    client: &reqwest::Client,
    model: &str,
    api_key: &str,
    chunks: &[ChunkBatchEntry],
) -> Result<(Vec<ChunkExtraction>, u64, u64)> {
    // Returns (extractions, input_tokens, output_tokens)
    let user_msg = build_prompt(chunks);

    let body = serde_json::json!({
        "model": model,
        "messages": [
            {"role": "system", "content": SYSTEM_PROMPT},
            {"role": "user", "content": user_msg}
        ],
        "temperature": 0.0,
        "max_tokens": 1000,
    });

    let url = "https://api.openai.com/v1/chat/completions";

    for attempt in 1..=2 {
        debug!("LLM NER attempt {} for {} chunks", attempt, chunks.len());

        let response = client
            .post(url)
            .header("Authorization", format!("Bearer {}", api_key))
            .header("Content-Type", "application/json")
            .json(&body)
            .timeout(std::time::Duration::from_secs(30))
            .send()
            .await;

        match response {
            Ok(resp) => {
                let status = resp.status();

                if (status == 429 || status.is_server_error()) && attempt < 2 {
                    warn!("LLM NER API error (status {}), retrying...", status);
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                    continue;
                }

                if status.is_success() {
                    let text = resp
                        .text()
                        .await
                        .context("Failed to read LLM NER response")?;

                    let parsed: serde_json::Value = serde_json::from_str(&text)
                        .context("Failed to parse LLM NER response JSON")?;

                    let usage = &parsed["usage"];
                    let input_tokens = usage["prompt_tokens"].as_u64().unwrap_or(0);
                    let output_tokens = usage["completion_tokens"].as_u64().unwrap_or(0);

                    let content = parsed["choices"][0]["message"]["content"]
                        .as_str()
                        .unwrap_or("{}");

                    // Clean markdown code blocks if present
                    let clean = content
                        .trim()
                        .trim_start_matches("```json")
                        .trim_start_matches("```")
                        .trim_end_matches("```")
                        .trim();

                    match serde_json::from_str::<LlmNerResponse>(clean) {
                        Ok(ner) => {
                            debug!(
                                "LLM NER: extracted entities from {} chunks",
                                ner.chunks.len()
                            );
                            return Ok((ner.chunks, input_tokens, output_tokens));
                        }
                        Err(e) => {
                            warn!(
                                "LLM NER parse error: {} — content: {}",
                                e,
                                &clean[..clean.len().min(200)]
                            );
                            return Ok((Vec::new(), input_tokens, output_tokens));
                        }
                    }
                } else {
                    let error_text = resp.text().await.unwrap_or_default();
                    anyhow::bail!(
                        "LLM NER API failed with status {}: {}",
                        status,
                        &error_text[..error_text.len().min(200)]
                    );
                }
            }
            Err(e) => {
                warn!("LLM NER request failed (attempt {}): {}", attempt, e);
                if attempt < 2 {
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                    continue;
                }
                return Err(e).context("LLM NER request failed after retries");
            }
        }
    }

    anyhow::bail!("LLM NER failed after all retries")
}

/// Filter entities below confidence threshold.
pub fn filter_by_confidence(
    extractions: &[ChunkExtraction],
    threshold: f64,
) -> (Vec<ChunkExtraction>, Vec<(String, LlmEntity)>) {
    // Returns (filtered, rejected) where rejected = (chunk_id, entity)
    let mut filtered = Vec::new();
    let mut rejected = Vec::new();

    for chunk in extractions {
        let good_entities: Vec<LlmEntity> = chunk
            .entities
            .iter()
            .filter(|e| {
                if e.confidence < threshold {
                    rejected.push((chunk.chunk_id.clone(), (*e).clone()));
                    false
                } else {
                    true
                }
            })
            .cloned()
            .collect();

        let good_relations: Vec<LlmRelation> = chunk
            .relations
            .iter()
            .filter(|r| r.confidence >= threshold)
            .cloned()
            .collect();

        filtered.push(ChunkExtraction {
            chunk_id: chunk.chunk_id.clone(),
            entities: good_entities,
            relations: good_relations,
        });
    }

    (filtered, rejected)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_prompt_single_chunk() {
        let chunks = vec![(
            "id-1".to_string(),
            "Anna z HR przygotuje harmonogram.".to_string(),
            vec![],
        )];
        let prompt = build_prompt(&chunks);
        assert!(prompt.contains("[CHUNK id=\"id-1\""));
        assert!(prompt.contains("Anna z HR"));
        assert!(prompt.contains("[/CHUNK]"));
    }

    #[test]
    fn test_build_prompt_with_known_entities() {
        let chunks = vec![(
            "id-1".to_string(),
            "Jan powiedział...".to_string(),
            vec![("Jan Kowalski".to_string(), "Person".to_string())],
        )];
        let prompt = build_prompt(&chunks);
        assert!(prompt.contains("known_entities=\"Jan Kowalski\""));
    }

    #[test]
    fn test_parse_llm_response() {
        let json = r#"{"chunks":[{"chunk_id":"id-1","entities":[{"name":"Anna","entity_type":"Person","confidence":0.85,"aliases":["Ania"]}],"relations":[{"subject":"Anna","relation":"works_at","object":"HR","confidence":0.8}]}]}"#;
        let resp: LlmNerResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.chunks.len(), 1);
        assert_eq!(resp.chunks[0].entities.len(), 1);
        assert_eq!(resp.chunks[0].entities[0].name, "Anna");
        assert_eq!(resp.chunks[0].relations.len(), 1);
    }

    #[test]
    fn test_filter_by_confidence() {
        let extractions = vec![ChunkExtraction {
            chunk_id: "id-1".to_string(),
            entities: vec![
                LlmEntity {
                    name: "Anna".into(),
                    entity_type: "Person".into(),
                    confidence: 0.9,
                    aliases: vec![],
                },
                LlmEntity {
                    name: "Maybe".into(),
                    entity_type: "Person".into(),
                    confidence: 0.5,
                    aliases: vec![],
                },
            ],
            relations: vec![
                LlmRelation {
                    subject: "A".into(),
                    relation: "r".into(),
                    object: "B".into(),
                    confidence: 0.8,
                },
                LlmRelation {
                    subject: "C".into(),
                    relation: "r".into(),
                    object: "D".into(),
                    confidence: 0.3,
                },
            ],
        }];

        let (filtered, rejected) = filter_by_confidence(&extractions, 0.7);
        assert_eq!(filtered[0].entities.len(), 1);
        assert_eq!(filtered[0].entities[0].name, "Anna");
        assert_eq!(filtered[0].relations.len(), 1);
        assert_eq!(rejected.len(), 1);
        assert_eq!(rejected[0].1.name, "Maybe");
    }

    #[test]
    fn test_estimate_tokens() {
        assert_eq!(estimate_tokens("hello world"), 3); // 11 chars / 4 + 1
        assert_eq!(estimate_tokens(""), 1);
    }
}
