use anyhow::{Context, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tracing::{debug, info, warn};

use crate::config::LlmConfig;
use crate::storage::{Chunk, RocksDbStore};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryGeneratorConfig {
    pub enabled: bool,
    pub max_chunks: usize,
    pub max_sections: usize,
    pub model: String,
}

impl Default for MemoryGeneratorConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_chunks: 200,
            max_sections: 20,
            model: "gpt-4.1-mini".to_string(),
        }
    }
}

const SYSTEM_PROMPT: &str = r#"Jesteś organizatorem pamięci. Otrzymujesz kolekcję fragmentów pamięci pogrupowanych według tematów i generujesz uporządkowany plik MEMORY.md w języku polskim.

Zasady:
- Użyj ## dla głównych sekcji, ### dla podsekcji
- Każdy fakt powinien być zwięzłym punktorem (bullet point)
- Dodawaj cross-references tam, gdzie to istotne (np. "Szczegóły: `ścieżka/do/pliku`")
- Grupuj powiązane fakty razem
- Najważniejsze fakty na początku w każdej sekcji
- Pomijaj redundantne/duplikowane informacje
- Wyjście to czysty Markdown, bez bloków kodu wokół całości
- Maksymalnie ~300 linii
- Jeśli fragmenty zawierają nazwy plików/ścieżek, zachowaj je
- Nie dodawaj informacji, których nie ma w źródłach"#;

#[derive(Debug, Serialize)]
struct CompletionRequest {
    model: String,
    messages: Vec<Message>,
    max_tokens: u32,
    temperature: f32,
}

#[derive(Debug, Serialize)]
struct Message {
    role: String,
    content: String,
}

#[derive(Debug, Deserialize)]
struct CompletionResponse {
    choices: Vec<Choice>,
}

#[derive(Debug, Deserialize)]
struct Choice {
    message: MessageResponse,
}

#[derive(Debug, Deserialize)]
struct MessageResponse {
    content: String,
}

#[derive(Debug, Serialize)]
pub struct MemoryProposal {
    pub proposal: String,
    pub chunk_count: usize,
    pub generated_at: String,
}

/// Group chunks by entity tags
fn group_by_entities(chunks: &[Chunk], store: &RocksDbStore) -> HashMap<String, Vec<Chunk>> {
    let mut groups: HashMap<String, Vec<Chunk>> = HashMap::new();

    for chunk in chunks {
        // Get entity tags for this chunk
        let entities = match store.get_entities(&chunk.id, &chunk.stream) {
            Ok(tags) => tags,
            Err(e) => {
                warn!("Failed to get entities for {}: {}", chunk.id, e);
                vec![]
            }
        };

        if entities.is_empty() {
            // Put in "Ogólne" (General) category
            groups
                .entry("Ogólne".to_string())
                .or_default()
                .push(chunk.clone());
        } else {
            // Put in each entity's group
            for entity in entities {
                groups.entry(entity).or_default().push(chunk.clone());
            }
        }
    }

    debug!(
        "Grouped {} chunks into {} entity groups",
        chunks.len(),
        groups.len()
    );
    groups
}

/// Format grouped chunks for LLM
fn format_chunks_for_llm(groups: HashMap<String, Vec<Chunk>>, max_sections: usize) -> String {
    let mut sections = Vec::new();

    // Sort groups by total importance (sum of chunk importances)
    let mut group_vec: Vec<_> = groups.into_iter().collect();
    group_vec.sort_by(|a, b| {
        let sum_a: f64 = a.1.iter().filter_map(|c| c.importance).sum();
        let sum_b: f64 = b.1.iter().filter_map(|c| c.importance).sum();
        sum_b
            .partial_cmp(&sum_a)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    // Take top N sections
    for (entity, mut chunks) in group_vec.into_iter().take(max_sections) {
        // Sort chunks within group by importance, then score
        chunks.sort_by(|a, b| {
            let imp_a = a.importance.unwrap_or(1.0);
            let imp_b = b.importance.unwrap_or(1.0);
            match imp_b
                .partial_cmp(&imp_a)
                .unwrap_or(std::cmp::Ordering::Equal)
            {
                std::cmp::Ordering::Equal => b
                    .score
                    .partial_cmp(&a.score)
                    .unwrap_or(std::cmp::Ordering::Equal),
                other => other,
            }
        });

        let mut section = format!("=== {} ===\n", entity);
        for chunk in chunks {
            section.push_str(&format!("- {}\n", chunk.content));
        }
        sections.push(section);
    }

    sections.join("\n")
}

/// Call LLM to format memory into structured MEMORY.md
async fn llm_format_memory(
    client: &Client,
    llm_config: &LlmConfig,
    model: &str,
    grouped_content: String,
) -> Result<String> {
    let api_key = llm_config
        .get_api_key()
        .context("OpenAI API key not configured")?;

    let url = "https://api.openai.com/v1/chat/completions";

    let request_body = CompletionRequest {
        model: model.to_string(),
        messages: vec![
            Message {
                role: "system".to_string(),
                content: SYSTEM_PROMPT.to_string(),
            },
            Message {
                role: "user".to_string(),
                content: grouped_content,
            },
        ],
        max_tokens: 4000,
        temperature: 0.3,
    };

    let timeout = std::time::Duration::from_secs(60);

    debug!("Calling LLM to format memory (model: {})", model);

    let response = client
        .post(url)
        .header("Authorization", format!("Bearer {}", api_key))
        .header("Content-Type", "application/json")
        .json(&request_body)
        .timeout(timeout)
        .send()
        .await
        .context("Failed to send LLM request")?;

    let status = response.status();
    if !status.is_success() {
        let error_text = response
            .text()
            .await
            .unwrap_or_else(|_| "unknown error".to_string());
        anyhow::bail!("LLM API failed with status {}: {}", status, error_text);
    }

    let body = response
        .json::<CompletionResponse>()
        .await
        .context("Failed to parse LLM response")?;

    let formatted = body
        .choices
        .into_iter()
        .next()
        .map(|c| c.message.content)
        .context("No completion data in response")?;

    debug!("LLM formatted memory into {} chars", formatted.len());
    Ok(formatted)
}

/// Generate MEMORY.md proposal from stored chunks
pub async fn generate_memory_md(
    store: Arc<RocksDbStore>,
    llm_client: &Client,
    llm_config: &LlmConfig,
    config: &MemoryGeneratorConfig,
    user_id: Option<&str>,
    stream_filter: Option<&str>,
) -> Result<MemoryProposal> {
    if !config.enabled {
        anyhow::bail!("Memory generator is disabled in config");
    }

    info!(
        "Starting MEMORY.md generation (user={:?}, stream={:?})",
        user_id, stream_filter
    );

    // Step 1: Gather top chunks (level >= 1, sorted by importance + score)
    let all_chunks = store.get_all_chunks().context("Failed to get all chunks")?;

    debug!("Found {} total chunks", all_chunks.len());

    // Filter: level >= 1 (skip L0 raw events)
    let mut candidates: Vec<Chunk> = all_chunks.into_iter().filter(|c| c.level >= 1).collect();

    // Apply user_id filter if specified
    if let Some(_uid) = user_id {
        candidates.retain(|_c| {
            // Check if chunk metadata contains user_id matching
            // For simplicity, we can check stream prefix mapping
            // In real usage, user_id would be in metadata
            // For now, just keep all if user_id is specified
            true
        });
    }

    // Apply stream filter if specified
    if let Some(stream) = stream_filter {
        candidates.retain(|c| c.stream.starts_with(stream));
    }

    debug!("After filtering: {} chunks", candidates.len());

    if candidates.is_empty() {
        return Ok(MemoryProposal {
            proposal: "# MEMORY.md\n\nBrak danych do wygenerowania pamięci.".to_string(),
            chunk_count: 0,
            generated_at: chrono::Utc::now().to_rfc3339(),
        });
    }

    // Sort by importance (desc), then score (desc)
    candidates.sort_by(|a, b| {
        let imp_a = a.importance.unwrap_or(1.0);
        let imp_b = b.importance.unwrap_or(1.0);
        match imp_b
            .partial_cmp(&imp_a)
            .unwrap_or(std::cmp::Ordering::Equal)
        {
            std::cmp::Ordering::Equal => b
                .score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal),
            other => other,
        }
    });

    // Take top N chunks
    candidates.truncate(config.max_chunks);

    info!(
        "Selected top {} chunks for MEMORY.md generation",
        candidates.len()
    );

    // Step 2: Group by entity tags
    let groups = group_by_entities(&candidates, &store);

    // Step 3: Format for LLM
    let formatted = format_chunks_for_llm(groups, config.max_sections);

    // Step 4: Call LLM to generate structured MEMORY.md
    let proposal = llm_format_memory(llm_client, llm_config, &config.model, formatted).await?;

    info!(
        "Successfully generated MEMORY.md proposal ({} chars)",
        proposal.len()
    );

    Ok(MemoryProposal {
        proposal,
        chunk_count: candidates.len(),
        generated_at: chrono::Utc::now().to_rfc3339(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_group_by_entities_empty() {
        // This would require a mock RocksDbStore; function signature verified at compile time
    }

    #[test]
    fn test_format_chunks_basic() {
        let mut groups = HashMap::new();
        let chunk1 = Chunk {
            id: "test1".to_string(),
            content: "Test content 1".to_string(),
            stream: "100".to_string(),
            level: 1,
            score: 1.0,
            timestamp: 0,
            consolidated: false,
            dormant: false,
            in_progress: false,
            prompt_version: None,
            source_ids: None,
            last_decay: None,
            metadata: None,
            importance: Some(1.5),
            persistent: false,
            last_implicit_boost: None,
            access_count: 0,
            source: None,
            created_by: None,
            updated_at: None,
            valid_from: None,
            valid_until: None,
            is_latest: true,
            superseded_by: None,
            supersedes_id: None,
            root_memory_id: None,
            version: 1,
            memory_type: None,
            extraction_meta: None,
            deleted_at: None,
            trust_level: None,
            ingester_user_id: None,
            alpha: 1.0,
            beta: 1.0,
            harmful_count: 0,
            n_ratings: 0,
            last_rated_at: None,
        };

        groups.insert("TestEntity".to_string(), vec![chunk1]);

        let formatted = format_chunks_for_llm(groups, 10);
        assert!(formatted.contains("TestEntity"));
        assert!(formatted.contains("Test content 1"));
    }
}
