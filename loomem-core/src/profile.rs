//! User profile synthesis.
//!
//! Generates a synthesized user profile from memories using a 3-layer
//! architecture: Pinned Facts (A), Stable Patterns (B), Current Focus (C).

use anyhow::{Context, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tracing::debug;

use crate::config::LlmConfig;
use crate::storage::{Chunk, RocksDbStore};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfileConfig {
    pub enabled: bool,
    pub model: String,
    pub max_chunks: usize,
    pub cache_ttl_secs: u64,
    pub max_static_facts: usize,
    pub max_recent_items: usize,
}

impl Default for ProfileConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            model: "gpt-4.1-mini".to_string(),
            max_chunks: 100,
            cache_ttl_secs: 3600,
            max_static_facts: 30,
            max_recent_items: 15,
        }
    }
}

const PROFILE_PROMPT: &str = r#"You are a profile synthesizer for a personal memory system.

You receive THREE LAYERS of information about a user:

LAYER A — PINNED FACTS (user-confirmed, highest priority):
These are facts the user has explicitly verified. They MUST appear
in the profile, verbatim or closely paraphrased. Never omit or
contradict pinned facts.

LAYER B — STABLE PATTERNS (auto-extracted, high confidence):
Recurring themes from long-term memory: expertise areas, work style,
relationships, long-term preferences. Synthesize into coherent
statements — do not list individual memories.

LAYER C — CURRENT FOCUS (recent, time-sensitive):
What's happening now. Active projects, recent decisions, open tasks.
Summarize at project level, not task level.

OUTPUT (JSON, all fields required, 2-4 sentences each):
{
  "identity": "Name, location, role, company, key relationships",
  "expertise": "Professional skills, domain knowledge, what they're known for",
  "projects": "Active projects with current phase — high-level only",
  "preferences": "Work style, decision patterns, tool preferences, communication style",
  "interests": "Hobbies, personal interests, life context beyond work",
  "current_focus": "This week's priorities, open decisions, blockers",
  "summary": "2-3 sentence elevator pitch — who is this person?"
}

RULES:
- SYNTHESIZE. Combine related facts. Never bullet-list raw memories.
- PRIORITIZE layers: A > B > C. If A says "I'm a cyclist", include it
  even if C has zero cycling mentions.
- SEPARATE stable from ephemeral. "Runs Acme" → identity.
  "Debugging ECA-16" → current_focus.
- OMIT ticket numbers, commit hashes, implementation details.
- ALL 7 fields MUST be filled. If a field has insufficient data, write
  "Not enough information yet" rather than leaving it empty or inventing content.
- Language: match the dominant language of the memories.

ANTI-PATTERNS:
❌ "User decided to use K-means instead of linfa-clustering"
✅ "Prefers minimal dependencies and pragmatic, lean implementations"

❌ "Budget is 10,000-15,000 PLN for Mokotów rental"
✅ "Searching for housing in Warsaw (Mokotów preferred), mid-range budget"

❌ Leaving identity empty while filling current_focus with 10 sentences
✅ Balanced coverage across all fields

❌ Putting everything into summary and leaving other fields empty
✅ Distribute information across the correct semantic fields

Return ONLY the JSON object, no markdown, no code fences, no preamble."#;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UserProfile {
    #[serde(default)]
    pub identity: String,
    #[serde(default)]
    pub expertise: String,
    #[serde(default)]
    pub projects: String,
    #[serde(default)]
    pub preferences: String,
    #[serde(default)]
    pub interests: String,
    #[serde(default)]
    pub current_focus: String,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub generated_at: u64,
    #[serde(default)]
    pub stream: String,
    #[serde(default)]
    pub chunk_count: usize,
}

/// Generate a user profile for a given stream.
pub async fn generate_profile(
    client: &Client,
    llm_config: &LlmConfig,
    profile_config: &ProfileConfig,
    store: &RocksDbStore,
    stream: &str,
) -> Result<UserProfile> {
    // Collect chunks: is_latest=true, not dormant, not soft-deleted, from this stream.
    // SEC-memctx-stream-leak: tombstoned chunks must be excluded from Layer C
    // (Current Focus) input — without this gate the LLM summarisation prompt
    // ingests soft-deleted content and can resurrect it in `profile.current_focus`.
    let mut chunks: Vec<Chunk> = Vec::new();
    for level in 0..=1 {
        let prefix = format!("chunk:L{}:", level);
        for (_key, value) in store.prefix_scan(prefix.as_bytes()) {
            if let Ok(chunk) = store.decode_chunk(&value) {
                if chunk.stream == stream
                    && chunk.is_latest
                    && !chunk.dormant
                    && chunk.deleted_at.is_none()
                {
                    chunks.push(chunk);
                }
            }
        }
    }

    // Sort by importance (desc) then timestamp (desc)
    chunks.sort_by(|a, b| {
        let imp_a = a.importance.unwrap_or(0.0);
        let imp_b = b.importance.unwrap_or(0.0);
        imp_b
            .partial_cmp(&imp_a)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(b.timestamp.cmp(&a.timestamp))
    });
    chunks.truncate(profile_config.max_chunks);

    if chunks.is_empty() {
        return Ok(UserProfile {
            summary: "No memories found for this stream.".to_string(),
            generated_at: now_secs(),
            stream: stream.to_string(),
            ..Default::default()
        });
    }

    // === 3-Layer Source Collection ===

    // Layer A: Pinned facts (user-confirmed, highest priority)
    // Stored under prefix pin:{stream_id}:
    let mut layer_a: Vec<String> = Vec::new();
    let pin_prefix = format!("pin:{}:", stream);
    for (_key, value) in store.prefix_scan(pin_prefix.as_bytes()) {
        if let Ok(text) = String::from_utf8(value.to_vec()) {
            layer_a.push(text);
        }
    }

    // Layer B: Stable patterns (L1 high-importance, persistent, static type)
    let mut layer_b: Vec<String> = Vec::new();
    for chunk in &chunks {
        let is_stable = (chunk.level >= 1 && chunk.importance.unwrap_or(0.0) >= 1.0)
            || chunk.persistent
            || chunk.memory_type.as_deref() == Some("static");

        if is_stable {
            layer_b.push(chunk.content.clone());
        }
    }
    layer_b.truncate(profile_config.max_static_facts);

    // Layer C: Current focus (recent L0/L1, last 7 days)
    let seven_days_ago = now_secs().saturating_sub(7 * 86400);
    let mut layer_c: Vec<String> = Vec::new();
    for chunk in &chunks {
        let is_recent =
            chunk.timestamp >= seven_days_ago || chunk.memory_type.as_deref() == Some("dynamic");
        let is_stable = chunk.persistent || chunk.memory_type.as_deref() == Some("static");

        if is_recent && !is_stable {
            layer_c.push(chunk.content.clone());
        }
    }
    layer_c.truncate(profile_config.max_recent_items);

    // Load profile feedback (if any)
    let mut feedback_items: Vec<String> = Vec::new();
    let fb_prefix = format!("profile_feedback:{}:", stream);
    for (_key, value) in store.prefix_scan(fb_prefix.as_bytes()) {
        if let Ok(text) = String::from_utf8(value.to_vec()) {
            feedback_items.push(text);
        }
    }

    // Build layered input for LLM
    let layer_a_text = if layer_a.is_empty() {
        "(no pinned facts yet)".to_string()
    } else {
        layer_a.join("\n")
    };

    let layer_b_text = if layer_b.is_empty() {
        "(no stable patterns found)".to_string()
    } else {
        layer_b.join("\n")
    };

    let layer_c_text = if layer_c.is_empty() {
        "(no recent activity)".to_string()
    } else {
        layer_c.join("\n")
    };

    let feedback_text = if feedback_items.is_empty() {
        "(no user feedback)".to_string()
    } else {
        feedback_items.join("\n")
    };

    let memories_text = format!(
        "=== LAYER A: PINNED FACTS ===\n{}\n\n=== LAYER B: STABLE PATTERNS ===\n{}\n\n=== LAYER C: CURRENT FOCUS ===\n{}\n\n=== USER FEEDBACK ===\n{}",
        layer_a_text, layer_b_text, layer_c_text, feedback_text
    );

    // Call LLM
    let api_key = llm_config
        .get_api_key()
        .context("OpenAI API key not configured for profile generation")?;

    let request_body = serde_json::json!({
        "model": &profile_config.model,
        "messages": [
            {"role": "system", "content": PROFILE_PROMPT},
            {"role": "user", "content": memories_text}
        ],
        "max_tokens": 2000,
        "temperature": 0.0
    });

    let response = client
        .post("https://api.openai.com/v1/chat/completions")
        .header("Authorization", format!("Bearer {}", api_key))
        .header("Content-Type", "application/json")
        .json(&request_body)
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .context("Failed to send profile generation request")?;

    let status = response.status();
    if !status.is_success() {
        let error_text = response.text().await.unwrap_or_default();
        anyhow::bail!("Profile LLM call failed ({}): {}", status, error_text);
    }

    #[derive(Deserialize)]
    struct LlmResponse {
        choices: Vec<LlmChoice>,
    }
    #[derive(Deserialize)]
    struct LlmChoice {
        message: LlmMessage,
    }
    #[derive(Deserialize)]
    struct LlmMessage {
        content: String,
    }

    let llm_resp: LlmResponse = response
        .json()
        .await
        .context("Failed to parse profile LLM response")?;

    let content = llm_resp
        .choices
        .first()
        .map(|c| c.message.content.clone())
        .unwrap_or_default();

    let mut profile: UserProfile =
        parse_profile_json(&content).context("Failed to parse profile JSON from LLM")?;

    profile.generated_at = now_secs();
    profile.stream = stream.to_string();
    profile.chunk_count = chunks.len();

    Ok(profile)
}

/// Sanitize and parse LLM output into a UserProfile.
///
/// Handles common LLM quirks: markdown fences, preamble text before JSON,
/// trailing text after JSON, BOM characters, etc.
fn parse_profile_json(raw: &str) -> Result<UserProfile> {
    let trimmed = raw.trim().trim_start_matches('\u{feff}');

    // Strip markdown code fences
    let stripped = trimmed
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();

    // Try direct parse first
    if let Ok(profile) = serde_json::from_str::<UserProfile>(stripped) {
        return Ok(profile);
    }

    // Try to extract JSON object by finding first { and last }
    if let (Some(start), Some(end)) = (stripped.find('{'), stripped.rfind('}')) {
        let json_slice = &stripped[start..=end];
        if let Ok(profile) = serde_json::from_str::<UserProfile>(json_slice) {
            return Ok(profile);
        }
    }

    // Last resort: try on the original raw content
    if let (Some(start), Some(end)) = (raw.find('{'), raw.rfind('}')) {
        let json_slice = &raw[start..=end];
        if let Ok(profile) = serde_json::from_str::<UserProfile>(json_slice) {
            return Ok(profile);
        }
    }

    anyhow::bail!(
        "Could not extract valid JSON from LLM response. First 200 chars: {}",
        &raw.chars().take(200).collect::<String>()
    )
}

/// Format profile as markdown for system prompt injection.
pub fn profile_to_markdown(profile: &UserProfile) -> String {
    let mut md = String::new();

    if !profile.summary.is_empty() {
        md.push_str("## Profile\n\n");
        md.push_str(&profile.summary);
        md.push('\n');
    }

    let sections = [
        ("Identity", &profile.identity),
        ("Expertise", &profile.expertise),
        ("Projects", &profile.projects),
        ("Preferences", &profile.preferences),
        ("Interests", &profile.interests),
        ("Current Focus", &profile.current_focus),
    ];

    for (title, content) in sections {
        if !content.is_empty() {
            md.push_str(&format!("\n### {}\n\n{}\n", title, content));
        }
    }

    md
}

/// Get cached profile or generate new one.
pub async fn get_or_generate_profile(
    client: &Client,
    llm_config: &LlmConfig,
    profile_config: &ProfileConfig,
    store: &RocksDbStore,
    stream: &str,
    data_dir: &Path,
    force_refresh: bool,
) -> Result<UserProfile> {
    let cache_path = profile_cache_path(data_dir, stream);

    // Check cache (unless force refresh or dirty)
    if !force_refresh {
        if let Some(cached) = load_cached_profile(&cache_path, profile_config.cache_ttl_secs)? {
            // Check dirty flag
            let dirty_key = format!("profile_dirty:{}", stream);
            let is_dirty = store.get(dirty_key.as_bytes())?.is_some();

            if !is_dirty {
                debug!(
                    "Returning cached profile for stream {} (age: {}s)",
                    stream,
                    now_secs() - cached.generated_at
                );
                return Ok(cached);
            }
            debug!("Profile cache dirty for stream {}, regenerating", stream);
        }
    }

    // Generate new profile
    let profile = generate_profile(client, llm_config, profile_config, store, stream).await?;

    // Cache to file
    save_profile_cache(&cache_path, &profile)?;

    // Clear dirty flag
    let dirty_key = format!("profile_dirty:{}", stream);
    let _ = store.delete(dirty_key.as_bytes());

    Ok(profile)
}

/// Set the dirty flag for a stream's profile cache.
pub fn mark_profile_dirty(store: &RocksDbStore, stream: &str) -> Result<()> {
    let dirty_key = format!("profile_dirty:{}", stream);
    store.put(dirty_key.as_bytes(), b"1")?;
    Ok(())
}

fn profile_cache_path(data_dir: &Path, stream: &str) -> PathBuf {
    data_dir
        .join("profiles")
        .join(format!("stream_{}.json", stream))
}

fn load_cached_profile(path: &Path, ttl_secs: u64) -> Result<Option<UserProfile>> {
    if !path.exists() {
        return Ok(None);
    }

    let content = std::fs::read_to_string(path).context("Failed to read profile cache")?;

    let profile: UserProfile = match serde_json::from_str(&content) {
        Ok(p) => p,
        Err(_) => {
            // Cache has old format — treat as expired so it gets regenerated
            debug!("Profile cache format mismatch, treating as expired");
            return Ok(None);
        }
    };

    let age = now_secs().saturating_sub(profile.generated_at);
    if age > ttl_secs {
        debug!("Profile cache expired (age: {}s, ttl: {}s)", age, ttl_secs);
        return Ok(None);
    }

    Ok(Some(profile))
}

fn save_profile_cache(path: &Path, profile: &UserProfile) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).context("Failed to create profiles directory")?;
    }

    let json = serde_json::to_string_pretty(profile).context("Failed to serialize profile")?;

    std::fs::write(path, json).context("Failed to write profile cache")?;

    Ok(())
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
