//! Frozen E4 algorithm: query-aware rank-stable redundancy deferral.
//!
//! This policy is fixed before quality-label inspection. It uses only the query
//! and complete E0 records: at least five distinct five-token shingles, 80%
//! candidate-shingle containment in one retained record, query-match inclusion,
//! no novel normalized tokens, and identical ordered numeric/negation context
//! windows (two neighboring tokens each side). Currency symbols are tokens.
//! This conservative amendment followed gold-blind synthetic review, before
//! source freeze or quality-label inspection. It is not semantic equivalence:
//! reordered ordinary words or differences outside these guards may still matter.
//! Deferred records retain their
//! relative order at the tail. No content, score, ID, or record count changes.
//! Bounds are full-record bounds, never prefix truncation. Any limit violation
//! returns the original order. Exact reader token budgeting remains downstream.
use std::collections::BTreeSet;

use crate::HybridSearchResult;

const MAX_CANDIDATES: usize = 60;
const MAX_RECORD_BYTES: usize = 16 * 1024;
const MAX_TOTAL_BYTES: usize = 512 * 1024;
const MAX_QUERY_BYTES: usize = 16 * 1024;
const MAX_QUERY_TERMS: usize = 16;
const SHINGLE_WIDTH: usize = 5;
const MIN_SHINGLES: usize = 5;

struct Profile {
    shingles: BTreeSet<String>,
    token_set: BTreeSet<String>,
    query_matches: BTreeSet<String>,
    critical: Vec<String>,
}

/// Reorder bounded primary candidates by deferring redundant complete records.
/// The first record is always retained, and the output is a permutation of input.
#[must_use]
pub fn select(query: &str, candidates: Vec<HybridSearchResult>) -> Vec<HybridSearchResult> {
    if !within_bounds(query, &candidates) {
        return candidates;
    }
    let query_terms: BTreeSet<_> = tokens(query)
        .into_iter()
        .filter(|token| !boilerplate(token))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .take(MAX_QUERY_TERMS)
        .collect();
    if query_terms.is_empty() {
        return candidates;
    }
    let mut retained = Vec::new();
    let mut deferred = Vec::new();
    let mut profiles = Vec::new();
    for candidate in candidates {
        let profile = profile(&candidate.content, &query_terms);
        if profiles
            .iter()
            .any(|previous| redundant(&profile, previous))
        {
            deferred.push(candidate);
        } else {
            profiles.push(profile);
            retained.push(candidate);
        }
    }
    retained.extend(deferred);
    retained
}

fn within_bounds(query: &str, candidates: &[HybridSearchResult]) -> bool {
    query.len() <= MAX_QUERY_BYTES
        && candidates.len() <= MAX_CANDIDATES
        && candidates
            .iter()
            .all(|item| item.content.len() <= MAX_RECORD_BYTES)
        && candidates
            .iter()
            .map(|item| item.content.len())
            .sum::<usize>()
            <= MAX_TOTAL_BYTES
}

fn tokens(text: &str) -> Vec<String> {
    text.split(|value: char| {
        !value.is_alphanumeric() && !matches!(value, '$' | '€' | '£' | '¥' | '₹' | '₽' | '₩')
    })
    .filter(|token| !token.is_empty())
    .map(str::to_lowercase)
    .collect()
}

fn profile(content: &str, query: &BTreeSet<String>) -> Profile {
    let tokens = tokens(content);
    let shingles = tokens
        .windows(SHINGLE_WIDTH)
        .map(|words| words.join(" "))
        .collect();
    let query_matches = tokens
        .iter()
        .filter(|word| query.contains(*word))
        .cloned()
        .collect();
    let critical = critical_contexts(&tokens);
    let token_set = tokens.into_iter().collect();
    Profile {
        shingles,
        token_set,
        query_matches,
        critical,
    }
}

fn redundant(candidate: &Profile, previous: &Profile) -> bool {
    candidate.shingles.len() >= MIN_SHINGLES
        && candidate.token_set.is_subset(&previous.token_set)
        && candidate.critical == previous.critical
        && candidate.query_matches.is_subset(&previous.query_matches)
        && candidate.shingles.intersection(&previous.shingles).count() * 5
            >= candidate.shingles.len() * 4
}

fn critical_contexts(tokens: &[String]) -> Vec<String> {
    tokens
        .iter()
        .enumerate()
        .filter(|(_, token)| critical(token))
        .map(|(index, _)| {
            let start = index.saturating_sub(2);
            let end = (index + 3).min(tokens.len());
            // Preserve both sequence and the critical token's offset in its window.
            format!("{}:{}", index - start, tokens[start..end].join(" "))
        })
        .collect()
}

fn critical(token: &str) -> bool {
    token.chars().any(char::is_numeric)
        || matches!(
            token,
            "t" | "ain"
                | "nobody"
                | "nothing"
                | "nowhere"
                | "no"
                | "not"
                | "never"
                | "none"
                | "neither"
                | "nor"
                | "without"
                | "cannot"
                | "don"
                | "doesn"
                | "didn"
                | "won"
                | "wouldn"
                | "couldn"
                | "shouldn"
                | "isn"
                | "aren"
                | "wasn"
                | "weren"
                | "hasn"
                | "haven"
                | "hadn"
                | "mustn"
                | "nie"
                | "nigdy"
                | "bez"
                | "żaden"
                | "żadna"
                | "żadne"
                | "ani"
        )
}

fn boilerplate(token: &str) -> bool {
    matches!(
        token,
        "what"
            | "which"
            | "how"
            | "could"
            | "would"
            | "should"
            | "please"
            | "tell"
            | "give"
            | "me"
            | "my"
            | "the"
            | "a"
            | "an"
            | "for"
            | "about"
            | "do"
            | "did"
            | "can"
            | "i"
            | "is"
            | "was"
            | "jaki"
            | "jak"
            | "co"
            | "który"
            | "proszę"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = "I enjoy guitar practice every evening with friends after work and before dinner in the quiet music room";

    fn record(id: usize, content: &str) -> HybridSearchResult {
        HybridSearchResult {
            id: id.to_string(),
            content: content.into(),
            user_id: String::new(),
            app_id: String::new(),
            level: 0,
            timestamp: 0,
            score: 1.0,
            bm25_score: 1.0,
            vector_score: 0.0,
            time_decay_factor: 1.0,
        }
    }

    fn ids(records: &[HybridSearchResult]) -> Vec<&str> {
        records.iter().map(|record| record.id.as_str()).collect()
    }

    #[test]
    fn defers_duplicate_and_exposes_complementary_evidence_early() {
        let input = vec![
            record(0, BASE),
            record(1, BASE),
            record(2, "My guitar budget is 400 euros"),
        ];
        let output = select("guitar budget", input.clone());
        assert_eq!(ids(&output), ["0", "2", "1"]);
        for original in input {
            let returned = output.iter().find(|item| item.id == original.id).unwrap();
            assert_eq!(
                serde_json::to_value(returned).unwrap(),
                serde_json::to_value(original).unwrap()
            );
        }
    }

    #[test]
    fn extra_query_match_preserves_complementary_near_duplicate() {
        let extra = format!("{BASE} budget");
        let input = vec![
            record(0, BASE),
            record(1, &extra),
            record(2, "Distinct evidence"),
        ];
        assert_eq!(ids(&select("guitar budget", input)), ["0", "1", "2"]);
    }

    #[test]
    fn changed_numeric_and_negation_token_guards() {
        for (left, right) in [
            ("2026 10 19", "2026 10 20"),
            ("400", "500"),
            ("", "not"),
            ("10", "10 10"),
            ("", "don't"),
            ("", "can't"),
        ] {
            let input = vec![
                record(0, &format!("{BASE} {left}")),
                record(1, &format!("{BASE} {right}")),
                record(2, "Distinct evidence"),
            ];
            assert_eq!(ids(&select("guitar", input)), ["0", "1", "2"]);
        }
    }

    #[test]
    fn new_fact_tokens_and_currency_symbols_keep_rank() {
        for (left, right) in [
            ("Alice", "Bob"),
            ("Monday", "Tuesday"),
            ("January", "February"),
            ("red", "blue"),
            ("$", "€"),
        ] {
            let input = vec![
                record(0, &format!("{BASE} {left}")),
                record(1, &format!("{BASE} {right}")),
                record(2, "Other evidence"),
            ];
            assert_eq!(ids(&select("guitar", input)), ["0", "1", "2"]);
        }
    }

    #[test]
    fn critical_order_and_local_attachment_keep_rank() {
        for (left, right) in [
            ("2026-10-19", "2026-19-10"),
            ("not piano but guitar", "piano but not guitar"),
            ("paid 400 then 500", "paid 500 then 400"),
        ] {
            let input = vec![
                record(0, &format!("{BASE} {left}")),
                record(1, &format!("{BASE} {right}")),
                record(2, "Other evidence"),
            ];
            assert_eq!(ids(&select("guitar", input)), ["0", "1", "2"]);
        }
    }

    #[test]
    fn tiny_repetitive_and_distinct_records_keep_rank() {
        for text in [
            "guitar",
            "guitar guitar guitar guitar guitar guitar",
            "guitar budget advice",
        ] {
            let input = vec![record(0, text), record(1, text), record(2, BASE)];
            assert_eq!(ids(&select("guitar", input)), ["0", "1", "2"]);
        }
    }

    #[test]
    fn deterministic_normalization_and_stable_deferred_order() {
        let input = vec![
            record(0, BASE),
            record(1, &BASE.to_uppercase()),
            record(2, "Complementary guitar budget"),
            record(3, BASE),
        ];
        for _ in 0..3 {
            assert_eq!(ids(&select("guitar", input.clone())), ["0", "2", "1", "3"]);
        }
    }

    #[test]
    fn all_resource_limits_fail_open() {
        let duplicate = vec![record(0, BASE), record(1, BASE), record(2, "Other")];
        let mut cases = vec![(
            "guitar".to_string(),
            (0..61).map(|i| record(i, BASE)).collect(),
        )];
        cases.push(("x".repeat(MAX_QUERY_BYTES + 1), duplicate));
        cases.push((
            "guitar".into(),
            vec![
                record(0, BASE),
                record(1, &"x".repeat(MAX_RECORD_BYTES + 1)),
            ],
        ));
        cases.push((
            "guitar".into(),
            (0..33)
                .map(|i| record(i, &"x".repeat(MAX_RECORD_BYTES)))
                .collect(),
        ));
        for (query, input) in cases {
            let before = serde_json::to_value(&input).unwrap();
            assert_eq!(serde_json::to_value(select(&query, input)).unwrap(), before);
        }
    }

    #[test]
    fn empty_query_fails_open() {
        let input = vec![record(0, BASE), record(1, BASE), record(2, "Other")];
        assert_eq!(ids(&select("what should I", input)), ["0", "1", "2"]);
    }
}
