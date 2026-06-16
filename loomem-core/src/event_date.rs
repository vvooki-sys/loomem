//! cycle/151 (port of /114b2) — fast LLM event-date extraction for direct
//! `memory_store`.
//!
//! `extract_knowledge` (memory_extractor.rs) already extracts event_date for
//! conversation-ingestion paths via `THREE DATES MODEL`. Direct
//! `memory_store` (MCP `tool_store`) bypasses that pipeline — content arrives
//! as a single self-contained fact with no surrounding transcript. This
//! module fills the gap with a focused 1-call LLM probe.
//!
//! Design constraints:
//! - Opt-in: gated behind `LOOMEM_EVENT_DATE_EXTRACTION` (default OFF) on top
//!   of `knowledge_extraction.enabled` — /151 scope extension per CLAUDE.md
//!   §9.5; the source commit gated only on the config flag.
//! - Hot-path: write latency budget bounded by a 10s timeout cap.
//! - Silent failure: any error → `None`, caller falls back to ingest
//!   timestamp.
//! - Cost: short prompt + null-or-date response — ~50 input + ~20 output
//!   tokens per call, ~$0.0001 on gpt-4o-mini. No response caching: every
//!   `memory_store` content is assumed unique, so a cache would only add a
//!   staleness hazard (see /90 cache-key-by-model lesson) without hits.

use anyhow::Result;
use chrono::{Datelike, NaiveDate};
use reqwest::Client;
use serde::Deserialize;
use std::time::Duration;
use tracing::{debug, warn};

use crate::llm::LlmConfig;

/// /151 scope extension (CLAUDE.md §9.5): the LLM probe in the
/// `memory_store` write path is opt-in. Set to `1`/`true`/`yes` to enable.
pub const EVENT_DATE_EXTRACTION_ENV: &str = "LOOMEM_EVENT_DATE_EXTRACTION";

/// Whether the `memory_store` event-date LLM probe is enabled. Default OFF —
/// requires both this env var and `knowledge_extraction.enabled` (checked at
/// the call site) to be on.
pub fn extraction_enabled() -> bool {
    extraction_enabled_value(std::env::var(EVENT_DATE_EXTRACTION_ENV).ok().as_deref())
}

/// Pure core of [`extraction_enabled`] — testable without mutating process
/// env (MCP test env-var serialization lesson, /95).
fn extraction_enabled_value(v: Option<&str>) -> bool {
    matches!(
        v.map(str::to_ascii_lowercase).as_deref(),
        Some("1" | "true" | "yes")
    )
}

const EVENT_DATE_PROMPT: &str = r#"Extract the absolute date of the event described in the fact below.

Rules:
1. Return JSON: {"event_date": "YYYY-MM-DD"} OR {"event_date": null}
2. Only return a date if the fact explicitly refers to an event occurring on a specific date (birthday, anniversary, meeting, deployment, decision date).
3. For timeless preferences and biographical facts ("X likes Y", "X works at Z", "X lives in W"), return null.
4. For relative expressions ("yesterday", "last month", "in March"), resolve to ISO YYYY-MM-DD using the anchor date.
5. The anchor date for relative expressions is: {anchor_date}
6. Preserve four-digit years exactly. Do not guess or extrapolate.

Return ONLY the JSON, no markdown."#;

#[derive(Debug, Deserialize)]
struct EventDateResponse {
    event_date: Option<String>,
}

/// Probe `content` for an absolute event date. Returns the parsed
/// `NaiveDate` on success, `None` for timeless facts, LLM failures, or
/// missing API key. Anchor date is used to resolve relative expressions
/// like "yesterday" — pass today's ISO date for live-ingest paths.
///
/// Skips the LLM call entirely when:
/// - `content.len() < 8` (too short to encode a meaningful date claim)
/// - API key unavailable (silent fallback, no error returned)
pub async fn extract_event_date(
    client: &Client,
    llm_config: &LlmConfig,
    model: &str,
    content: &str,
    anchor_date: &str,
) -> Option<NaiveDate> {
    if content.trim().len() < 8 {
        return None;
    }
    let api_key = llm_config.get_api_key()?;
    let prompt = EVENT_DATE_PROMPT.replace("{anchor_date}", anchor_date);

    match call_llm(client, &api_key, model, &prompt, content).await {
        Ok(Some(date)) => Some(date),
        Ok(None) => None,
        Err(e) => {
            warn!("extract_event_date: LLM call failed, falling back to None: {e}");
            None
        }
    }
}

async fn call_llm(
    client: &Client,
    api_key: &str,
    model: &str,
    system_prompt: &str,
    content: &str,
) -> Result<Option<NaiveDate>> {
    let body = serde_json::json!({
        "model": model,
        "messages": [
            {"role": "system", "content": system_prompt},
            {"role": "user", "content": content}
        ],
        "max_tokens": 60,
        "temperature": 0.0,
    });

    let resp = client
        .post("https://api.openai.com/v1/chat/completions")
        .header("Authorization", format!("Bearer {api_key}"))
        .header("Content-Type", "application/json")
        .json(&body)
        .timeout(Duration::from_secs(10))
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        anyhow::bail!("event_date API {status}: {text}");
    }

    #[derive(Deserialize)]
    struct Wire {
        choices: Vec<Choice>,
    }
    #[derive(Deserialize)]
    struct Choice {
        message: Msg,
    }
    #[derive(Deserialize)]
    struct Msg {
        content: String,
    }

    let parsed: Wire = resp.json().await?;
    let raw = parsed
        .choices
        .into_iter()
        .next()
        .map(|c| c.message.content)
        .unwrap_or_default();

    let json_str = raw
        .trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();

    let decoded: EventDateResponse = serde_json::from_str(json_str)?;
    Ok(decoded.event_date.as_deref().and_then(parse_and_validate))
}

/// Parse `YYYY-MM-DD` and clamp to a plausible year range. Defends against
/// LLM hallucinations like `0001-01-01` or `9999-12-31`.
fn parse_and_validate(s: &str) -> Option<NaiveDate> {
    let date = NaiveDate::parse_from_str(s.trim(), "%Y-%m-%d").ok()?;
    if !(1900..=2100).contains(&date.year()) {
        debug!(
            "extract_event_date: rejecting out-of-range year {}",
            date.year()
        );
        return None;
    }
    Some(date)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_and_validate_accepts_recent_iso_date() {
        let d = parse_and_validate("1992-12-01").unwrap();
        assert_eq!(d.format("%Y-%m-%d").to_string(), "1992-12-01");
    }

    #[test]
    fn parse_and_validate_rejects_year_before_1900() {
        assert!(parse_and_validate("1856-03-15").is_none());
    }

    #[test]
    fn parse_and_validate_rejects_year_after_2100() {
        assert!(parse_and_validate("2200-01-01").is_none());
    }

    #[test]
    fn parse_and_validate_rejects_unparseable() {
        assert!(parse_and_validate("not a date").is_none());
        assert!(parse_and_validate("").is_none());
        assert!(parse_and_validate("2026-13-45").is_none());
    }

    #[test]
    fn parse_and_validate_accepts_whitespace() {
        assert!(parse_and_validate("  2026-03-15  ").is_some());
    }

    // /151 scope extension: env gate is default OFF and accepts only
    // explicit truthy values. Pure-value test — no env mutation (/95 rule).
    #[test]
    fn extraction_enabled_defaults_off() {
        assert!(!extraction_enabled_value(None));
        assert!(!extraction_enabled_value(Some("")));
        assert!(!extraction_enabled_value(Some("0")));
        assert!(!extraction_enabled_value(Some("false")));
        assert!(extraction_enabled_value(Some("1")));
        assert!(extraction_enabled_value(Some("true")));
        assert!(extraction_enabled_value(Some("YES")));
    }
}
