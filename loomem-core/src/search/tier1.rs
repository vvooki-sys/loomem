//! Tier-1 direct entity→attribute lookup (WS-1c, Spectron gap brief
//! 2026-07-09).
//!
//! Cheapest rung of the retrieval cost ladder: when a factual query names a
//! known graph entity plus at least one attribute term, the answer can be
//! served straight from the entity's chunk index — no embeddings, no fusion,
//! no LLM. This module holds the *pure* half of the tier: the config gate and
//! the query-shape helpers (attribute-term extraction + candidate content
//! matching). Orchestration — graph lookup, chunk loading, filters, response
//! assembly — lives in `loomem-server/src/handlers/search.rs::tier1_lookup`,
//! which falls back to the full fusion pipeline whenever this tier does not
//! confidently apply.
//!
//! Behind `[search.tier1] enabled` (default **off**): with the flag off no
//! tier code runs on the hot path and the execution path is identical to
//! pre-cycle behaviour.

use serde::{Deserialize, Serialize};

/// Sub-config for the Tier-1 direct lookup. Composed into `SearchConfig` as
/// `tier1` (same pattern as `rare_term_lane` / `graph` / `cache`).
/// `#[serde(default)]` on every field keeps existing `config.toml` files and
/// persisted state deserializable.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tier1Config {
    /// Master switch. Default `false` — tier fully inert.
    #[serde(default)]
    pub enabled: bool,
    /// Maximum entity chunks inspected per query. Entity chunk lists grow
    /// unbounded over time; the cap keeps the tier's cost strictly O(cap)
    /// chunk reads.
    #[serde(default = "default_candidate_cap")]
    pub candidate_cap: usize,
}

fn default_candidate_cap() -> usize {
    64
}

impl Default for Tier1Config {
    fn default() -> Self {
        Self {
            enabled: false,
            candidate_cap: default_candidate_cap(),
        }
    }
}

/// Interrogatives, copulas and glue words (PL + EN) that never carry the
/// *attribute* of an entity→attribute question. Conservative list — a missed
/// stopword only makes the tier more permissive about trying a match, and a
/// non-matching candidate set falls back to the full pipeline anyway.
fn is_query_stopword(token: &str) -> bool {
    matches!(
        token,
        // PL interrogatives / copulas / glue
        "jaki" | "jaka" | "jakie" | "jakiego" | "jakiej" | "jakim" | "jaką"
            | "który" | "która" | "które" | "którego" | "której" | "którym"
            | "kiedy" | "gdzie" | "dlaczego" | "czemu" | "czym" | "czego" | "kogo" | "komu"
            | "jest" | "był" | "była" | "było" | "byli" | "są" | "będzie" | "mają" | "miał"
            | "ile" | "czy" | "oraz" | "albo" | "lub" | "ale" | "dla" | "nad" | "pod" | "przy"
            | "się" | "ten" | "tego" | "tym" | "taki" | "takie"
            // EN interrogatives / copulas / glue
            | "what" | "which" | "when" | "where" | "why" | "who" | "whose" | "whom" | "how"
            | "the" | "and" | "was" | "were" | "has" | "have" | "had" | "does" | "did" | "are"
            | "his" | "her" | "its" | "their" | "this" | "that" | "with" | "about" | "from"
            | "many" | "much"
    )
}

/// Extract the attribute terms of an entity→attribute query: lowercase
/// alphanumeric tokens of length ≥ 3 that are neither query stopwords nor
/// part of any detected entity name. Order-preserving, deduplicated.
///
/// An empty return means the query carries no attribute besides the entity
/// itself (e.g. bare "Anna Kowalska") — callers must then fall back to the
/// full pipeline, where dense retrieval handles open-ended entity queries
/// better than a raw chunk dump would.
#[must_use]
pub fn attribute_terms(query: &str, entity_names: &[String]) -> Vec<String> {
    let entities_lower: Vec<String> = entity_names.iter().map(|e| e.to_lowercase()).collect();
    let mut seen = std::collections::HashSet::new();
    let mut terms = Vec::new();
    for raw in query.split(|c: char| !c.is_alphanumeric()) {
        let token = raw.to_lowercase();
        if token.chars().count() < 3 || is_query_stopword(&token) {
            continue;
        }
        if entities_lower.iter().any(|e| e.contains(token.as_str())) {
            continue;
        }
        if seen.insert(token.clone()) {
            terms.push(token);
        }
    }
    terms
}

/// Whether a candidate chunk's content answers the attribute part of the
/// query: case-insensitive containment of at least one attribute term.
/// Exact-substring on purpose (typed lookup, precision over recall) — an
/// inflection miss makes the tier decline and the full pipeline take over,
/// it never degrades the result set.
#[must_use]
pub fn content_matches(content: &str, terms: &[String]) -> bool {
    if terms.is_empty() {
        return false;
    }
    let content_lower = content.to_lowercase();
    terms.iter().any(|t| content_lower.contains(t.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config_is_off_with_brief_start_values() {
        let c = Tier1Config::default();
        assert!(!c.enabled);
        assert_eq!(c.candidate_cap, 64);
    }

    #[test]
    fn test_attribute_terms_strips_entities_stopwords_and_short_tokens() {
        let terms = attribute_terms(
            "Jaki jest numer telefonu Anny Kowalskiej?",
            &["Anny".to_string(), "Kowalskiej".to_string()],
        );
        assert_eq!(terms, vec!["numer".to_string(), "telefonu".to_string()]);
    }

    #[test]
    fn test_attribute_terms_entity_substring_tokens_are_excluded() {
        // Multi-word entity: each of its tokens is a substring of the
        // lowercased entity name and must not leak into attribute terms.
        let terms = attribute_terms(
            "what is Project Widmo deadline",
            &["Project Widmo".to_string()],
        );
        assert_eq!(terms, vec!["deadline".to_string()]);
    }

    #[test]
    fn test_attribute_terms_bare_entity_query_is_empty() {
        // Pure-entity query carries no attribute → caller must fall back.
        let terms = attribute_terms("Anna Kowalska?", &["Anna Kowalska".to_string()]);
        assert!(terms.is_empty(), "expected no terms, got {terms:?}");
    }

    #[test]
    fn test_attribute_terms_dedupes_preserving_order() {
        let terms = attribute_terms("deadline deadline budget", &[]);
        assert_eq!(terms, vec!["deadline".to_string(), "budget".to_string()]);
    }

    #[test]
    fn test_content_matches_requires_a_term_hit() {
        let terms = vec!["telefonu".to_string(), "numer".to_string()];
        assert!(content_matches("Numer telefonu Anny: [REDACTED]", &terms));
        assert!(!content_matches("Anna lubi kolarstwo górskie", &terms));
    }

    #[test]
    fn test_content_matches_empty_terms_never_match() {
        // Guard: an empty term list must not degenerate into match-everything.
        assert!(!content_matches("anything at all", &[]));
    }

    #[test]
    fn test_content_matches_is_case_insensitive() {
        let terms = vec!["deadline".to_string()];
        assert!(content_matches(
            "Project Widmo DEADLINE: 2026-08-01",
            &terms
        ));
    }
}
