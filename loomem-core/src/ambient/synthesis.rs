//! Deterministic plain-fact synthesis — Stage 2 of cycle/103a-mvp.
//!
//! Per §10.5.3 + B-2 fix: ambient `text` MUST be declarative, 3rd-person,
//! with no metawords ("according to memory", "based on memory", "I remember"
//! etc.) and no provenance citation in the agent-facing text. /103gate §8.6
//! identified per-result skepticism as the load-bearing failure mode; cited
//! form invites that skepticism layer.
//!
//! MVP scope per `cycles/cycle-103a-layer1-endpoint-brief.md` Q1=C:
//! - **High / Medium / Low tier** — single-source deterministic template:
//!   normalize first-person → third-person, strip leading metaword phrases,
//!   truncate at first sentence boundary, enforce 200-token per-snippet cap.
//! - **Conflict tier** — deterministic fallback template: "User has
//!   expressed conflicting preferences about X — said Y on date D1 and Z on
//!   date D2." LLM synthesis for conflict-tier deferred to /103a-full per
//!   `/103gate` §8 + 4-probe evidence.
//!
//! Stage 2 produces synthesis logic over already-retrieved candidates;
//! handler integration (Stage 3) wires it into `POST /v1/ambient`.

use std::sync::OnceLock;

use anyhow::{anyhow, Result};
use regex::Regex;

use crate::HybridSearchResult;

use super::cache::{count_tokens, TOKEN_PER_SNIPPET_CAP};
use super::types::{AmbientSnippet, Tier};

/// First-token replacement table used by `rewrite_first_person`. Order
/// matters — longer phrases must come before shorter prefixes (e.g. "I've"
/// before "I"). Each pattern is matched at the START of the trimmed text.
const FIRST_PERSON_PREFIXES: &[(&str, &str)] = &[
    ("I've ", "User has "),
    ("I'm ", "User is "),
    ("I'll ", "User will "),
    ("I'd ", "User would "),
    ("I have ", "User has "),
    ("I had ", "User had "),
    ("I am ", "User is "),
    ("I was ", "User was "),
    ("I will ", "User will "),
    ("I would ", "User would "),
    ("I do ", "User does "),
    ("I did ", "User did "),
    ("I ", "User "),
    ("My ", "User's "),
    ("my ", "user's "),
];

/// Patterns that MUST NOT appear anywhere in synthesized `text`. Per B-2 fix
/// (and /103gate §8.6): cited-memory framing re-engages the per-result
/// skepticism prior, defeating ambient injection's value-add.
fn metaword_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        // Single-line regex (raw-string backslash-newline does not escape line
        // breaks in Rust). Alternation covers the cited-memory phrase families
        // identified in /103gate §8.6 + B-2 fix + probe-1 cited-form analysis.
        let pattern = r"(?i)(according to (your|my|the) (memory|notes|records|background))|(based on (your|my|the) (memory|notes|records|background|stored))|(from (your|my|the) (memory|notes|records|background))|(i (remember|recall) (that|seeing|reading))|((your|my) memory (says|shows|indicates|tells|contains))";
        Regex::new(pattern).expect("metaword_regex compile")
    })
}

/// Synthesize a single positive ambient snippet from a retrieved candidate.
///
/// Steps (all deterministic, MVP):
/// 1. Trim whitespace.
/// 2. Truncate at first sentence boundary (`. `, `? `, `! `).
/// 3. Apply first-person → third-person rewrite via prefix table.
/// 4. Validate no metaword phrases remain (`validate_no_metawords`); fail
///    closed by returning `Err` if validation trips — caller drops the
///    snippet rather than emit a spec-violating ambient.
/// 5. Enforce per-snippet 200-token cap; oversized snippets returned as
///    `Err`, caller drops them silently (parallels
///    `cache::truncate_to_budget` policy).
pub fn synthesize_snippet(
    candidate: &HybridSearchResult,
    score: f32,
    tier: Tier,
) -> Result<AmbientSnippet> {
    let text = render_text(&candidate.content)?;
    Ok(AmbientSnippet { text, tier, score })
}

/// Render plain-fact text from a chunk's content. Returns `Err` when the
/// resulting text would violate the §10.5.3 constraints OR exceeds the
/// per-snippet token cap.
pub fn render_text(content: &str) -> Result<String> {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return Err(anyhow!("empty content cannot synthesize snippet"));
    }
    let first_sentence = first_sentence(trimmed);
    let rewritten = rewrite_first_person(first_sentence);
    if !validate_no_metawords(&rewritten) {
        return Err(anyhow!(
            "synthesized text violates plain-fact constraint (metaword detected)"
        ));
    }
    let n = count_tokens(&rewritten)?;
    if n > TOKEN_PER_SNIPPET_CAP {
        return Err(anyhow!(
            "synthesized snippet ({n} tokens) exceeds {TOKEN_PER_SNIPPET_CAP}-token cap"
        ));
    }
    Ok(rewritten)
}

/// Conflict-tier deterministic template per AC-4 + Q1=C MVP.
///
/// Inputs: `predicate` (the topic, e.g. "music streaming preference"),
/// `value_a` + `date_a` (older statement), `value_b` + `date_b` (newer
/// statement). LLM-flavoured conflict synthesis is deferred to /103a-full.
pub fn render_conflict_text(
    predicate: &str,
    value_a: &str,
    date_a: &str,
    value_b: &str,
    date_b: &str,
) -> Result<String> {
    let text = format!(
        "User has expressed conflicting preferences about {predicate} — said {value_a} on {date_a} and {value_b} on {date_b}.",
    );
    if !validate_no_metawords(&text) {
        return Err(anyhow!(
            "conflict template produced metaword (template bug)"
        ));
    }
    let n = count_tokens(&text)?;
    if n > TOKEN_PER_SNIPPET_CAP {
        return Err(anyhow!(
            "conflict snippet ({n} tokens) exceeds {TOKEN_PER_SNIPPET_CAP}-token cap"
        ));
    }
    Ok(text)
}

/// Synthesize a conflict-tier `AmbientSnippet`. `score` is supplied by the
/// caller (typically `min(c_a, c_b)` per §10.5.2 conflict semantics).
pub fn synthesize_conflict_snippet(
    predicate: &str,
    value_a: &str,
    date_a: &str,
    value_b: &str,
    date_b: &str,
    score: f32,
) -> Result<AmbientSnippet> {
    let text = render_conflict_text(predicate, value_a, date_a, value_b, date_b)?;
    Ok(AmbientSnippet {
        text,
        tier: Tier::Conflict,
        score,
    })
}

/// Cut at the first sentence boundary (period / question mark / exclamation
/// followed by whitespace or end-of-string). Falls back to the full input
/// when no terminator is found.
#[must_use]
pub fn first_sentence(text: &str) -> &str {
    let bytes = text.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        if matches!(b, b'.' | b'?' | b'!') {
            // Boundary: punctuation followed by ' ' / '\n' or at end-of-string.
            let after = bytes.get(i + 1);
            if matches!(after, None | Some(b' ') | Some(b'\n') | Some(b'\t')) {
                return &text[..=i];
            }
        }
    }
    text
}

/// Apply the `FIRST_PERSON_PREFIXES` rewrite table. Only the first matching
/// prefix is applied; the rest of the text is unchanged. Documented as MVP
/// — full NLP first→third rewriting is deferred to LLM synthesis in
/// /103a-full.
#[must_use]
pub fn rewrite_first_person(text: &str) -> String {
    for (prefix, replacement) in FIRST_PERSON_PREFIXES {
        if let Some(rest) = text.strip_prefix(*prefix) {
            return format!("{replacement}{rest}");
        }
    }
    text.to_string()
}

/// `true` when the text contains no metaword phrases per the §10.5.3 +
/// B-2-fix forbidden-pattern list.
#[must_use]
pub fn validate_no_metawords(text: &str) -> bool {
    !metaword_regex().is_match(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(content: &str) -> HybridSearchResult {
        HybridSearchResult {
            id: "c1".to_string(),
            content: content.to_string(),
            user_id: "u".to_string(),
            app_id: "a".to_string(),
            level: 0,
            timestamp: 0,
            score: 0.0,
            bm25_score: 0.0,
            vector_score: 0.0,
            time_decay_factor: 1.0,
        }
    }

    #[test]
    fn first_sentence_truncates_at_period() {
        assert_eq!(first_sentence("First. Second."), "First.");
        assert_eq!(first_sentence("Question? Answer."), "Question?");
        assert_eq!(first_sentence("Wow! More."), "Wow!");
    }

    #[test]
    fn first_sentence_passes_through_when_no_terminator() {
        assert_eq!(first_sentence("No terminator here"), "No terminator here");
    }

    #[test]
    fn first_sentence_handles_period_at_end() {
        // No trailing space — boundary still applies because at-end matches.
        assert_eq!(first_sentence("Just one sentence."), "Just one sentence.");
    }

    #[test]
    fn rewrite_first_person_handles_common_contractions() {
        assert_eq!(
            rewrite_first_person("I've created a playlist."),
            "User has created a playlist."
        );
        assert_eq!(rewrite_first_person("I'm tired."), "User is tired.");
        assert_eq!(rewrite_first_person("I'll go."), "User will go.");
    }

    #[test]
    fn rewrite_first_person_handles_my_my() {
        assert_eq!(
            rewrite_first_person("My playlist is named 'Summer Vibes'."),
            "User's playlist is named 'Summer Vibes'."
        );
        assert_eq!(
            rewrite_first_person("my dentist is Dr. Smith."),
            "user's dentist is Dr. Smith."
        );
    }

    #[test]
    fn rewrite_first_person_passthrough_when_no_match() {
        // Already 3rd-person — no change.
        let s = "User redeemed a coupon at Target.";
        assert_eq!(rewrite_first_person(s), s);
    }

    #[test]
    fn validate_no_metawords_catches_cited_forms() {
        assert!(!validate_no_metawords(
            "According to your memory, you redeemed a coupon at Target."
        ));
        assert!(!validate_no_metawords(
            "Based on the memory, the answer is 42."
        ));
        assert!(!validate_no_metawords(
            "From your notes, the playlist is called 'Summer Vibes'."
        ));
        assert!(!validate_no_metawords("I remember that the answer is X."));
        assert!(!validate_no_metawords("Your memory says X."));
    }

    #[test]
    fn validate_no_metawords_accepts_plain_fact() {
        assert!(validate_no_metawords(
            "User's playlist is named 'Summer Vibes'."
        ));
        assert!(validate_no_metawords(
            "User redeemed a $5 coupon on coffee creamer at Target."
        ));
        assert!(validate_no_metawords(
            "User's daily commute takes 45 minutes each way."
        ));
    }

    #[test]
    fn render_text_first_person_to_plain_fact() {
        let out = render_text("I created a Spotify playlist called 'Summer Vibes'.").unwrap();
        assert_eq!(
            out,
            "User created a Spotify playlist called 'Summer Vibes'."
        );
    }

    #[test]
    fn render_text_strips_trailing_sentence() {
        let out = render_text("My dentist is Dr. Kowalski. The next visit is in March.").unwrap();
        assert_eq!(out, "User's dentist is Dr.");
        // Caveat: imperfect with "Dr." abbreviation — documented MVP limitation.
    }

    #[test]
    fn render_text_rejects_empty_content() {
        assert!(render_text("").is_err());
        assert!(render_text("   \n\t").is_err());
    }

    #[test]
    fn synthesize_snippet_attaches_tier_and_score() {
        let c = cand("I take yoga at Serenity Yoga.");
        let snip = synthesize_snippet(&c, 0.82, Tier::High).unwrap();
        assert_eq!(snip.tier, Tier::High);
        assert!((snip.score - 0.82).abs() < 1e-6);
        assert_eq!(snip.text, "User take yoga at Serenity Yoga.");
    }

    #[test]
    fn render_conflict_text_matches_template() {
        let t = render_conflict_text(
            "music streaming preference",
            "Spotify",
            "2026-04-01",
            "Apple Music",
            "2026-05-01",
        )
        .unwrap();
        assert!(t.contains("conflicting preferences about music streaming preference"));
        assert!(t.contains("Spotify"));
        assert!(t.contains("Apple Music"));
        assert!(t.contains("2026-04-01"));
        assert!(t.contains("2026-05-01"));
        // Template MUST NOT use cited-memory metawords.
        assert!(validate_no_metawords(&t));
    }

    #[test]
    fn synthesize_conflict_snippet_returns_conflict_tier() {
        let snip = synthesize_conflict_snippet(
            "lunch venue",
            "Mighty Bowl",
            "Mon",
            "Sushi Place",
            "Wed",
            0.68,
        )
        .unwrap();
        assert_eq!(snip.tier, Tier::Conflict);
        assert!((snip.score - 0.68).abs() < 1e-6);
    }

    #[test]
    fn render_text_rejects_when_metaword_remains_after_rewrite() {
        // Pathological: chunk content already contains a cited form that
        // even the rewrite table can't fix → render_text fails closed.
        let result = render_text("According to your memory, X.");
        assert!(result.is_err(), "metaword survives rewrite ⇒ Err");
    }

    #[test]
    fn render_text_rejects_oversized_snippet() {
        let huge = "word ".repeat(500); // ~500 tokens > 200 cap.
                                        // Add a period so first_sentence keeps the whole thing as one sentence.
        let huge = format!("{}.", huge.trim_end());
        let result = render_text(&huge);
        assert!(
            result.is_err(),
            "oversized snippet (>200 tok) must fail closed"
        );
    }
}
