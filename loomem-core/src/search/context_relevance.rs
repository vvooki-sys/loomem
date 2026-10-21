//! Frozen E5: raw-query relevance and density ordering of complete E0 records.
//!
//! Tokenization is Unicode alphanumeric splitting plus lowercase, without a
//! stoplist, stemming, query rewriting, or content-format parsing. Distinct query
//! terms present in the pool receive positive BM25 IDF:
//! ln(1 + (N - df + 0.5) / (df + 0.5)). Coverage is the matched IDF fraction.
//! Local BM25 uses k1=1.2, b=0.75 and complete-record token lengths, normalized
//! by the pool maximum. Final score is 0.6 coverage + 0.3 normalized BM25 +
//! 0.1 / (1 + original zero-based rank / 10). Record zero stays protected;
//! remaining records sort by descending score, then original rank.
//!
//! Fixed before quality-label inspection. Bounds fail open to original order;
//! absent query terms are ignored and no matched terms preserves original order.
//! This only permutes unchanged records. Exact reader token packing is downstream.
use std::collections::{BTreeMap, BTreeSet};

use crate::HybridSearchResult;

const MAX_CANDIDATES: usize = 60;
const MAX_RECORD_BYTES: usize = 16 * 1024;
const MAX_TOTAL_BYTES: usize = 512 * 1024;
const MAX_QUERY_BYTES: usize = 16 * 1024;
const MAX_QUERY_TERMS: usize = 64;

struct Profile {
    frequencies: BTreeMap<String, f64>,
    length: f64,
}

/// Reorder complete bounded candidates by query relevance, protecting record zero.
#[must_use]
pub fn select(query: &str, candidates: Vec<HybridSearchResult>) -> Vec<HybridSearchResult> {
    if !within_bounds(query, &candidates) || candidates.len() < 2 {
        return candidates;
    }
    let query: BTreeSet<_> = tokens(query).collect();
    if query.is_empty() || query.len() > MAX_QUERY_TERMS {
        return candidates;
    }
    let profiles: Vec<_> = candidates
        .iter()
        .map(|item| profile(&item.content))
        .collect();
    let count = profiles.iter().fold(0.0, |count, _| count + 1.0);
    let weights = query_weights(&query, &profiles, count);
    if weights.is_empty() {
        return candidates;
    }
    let average_length = profiles.iter().map(|item| item.length).sum::<f64>() / count;
    let lexical: Vec<_> = profiles
        .iter()
        .map(|item| bm25(item, &weights, average_length))
        .collect();
    let maximum = lexical.iter().copied().fold(0.0_f64, f64::max);
    let total_weight = weights.values().sum::<f64>();
    let mut rank = 0.0;
    let mut scored: Vec<_> = candidates
        .into_iter()
        .enumerate()
        .map(|(index, item)| {
            let coverage = weights
                .iter()
                .filter(|(term, _)| profiles[index].frequencies.contains_key(*term))
                .map(|(_, weight)| weight)
                .sum::<f64>()
                / total_weight;
            let score = 0.6 * coverage + 0.3 * lexical[index] / maximum + 0.1 / (1.0 + rank / 10.0);
            rank += 1.0;
            (index, score, item)
        })
        .collect();
    scored[1..].sort_by(|left, right| right.1.total_cmp(&left.1).then(left.0.cmp(&right.0)));
    scored.into_iter().map(|(_, _, item)| item).collect()
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

fn tokens(text: &str) -> impl Iterator<Item = String> + '_ {
    text.split(|value: char| !value.is_alphanumeric())
        .filter(|token| !token.is_empty())
        .map(str::to_lowercase)
}

fn profile(content: &str) -> Profile {
    let mut frequencies = BTreeMap::new();
    let mut length = 0.0;
    for token in tokens(content) {
        *frequencies.entry(token).or_insert(0.0) += 1.0;
        length += 1.0;
    }
    Profile {
        frequencies,
        length,
    }
}

fn query_weights(
    query: &BTreeSet<String>,
    profiles: &[Profile],
    count: f64,
) -> BTreeMap<String, f64> {
    query
        .iter()
        .filter_map(|term| {
            let frequency = profiles
                .iter()
                .filter(|item| item.frequencies.contains_key(term))
                .fold(0.0, |count, _| count + 1.0);
            (frequency > 0.0).then(|| {
                (
                    term.clone(),
                    ((count - frequency + 0.5) / (frequency + 0.5)).ln_1p(),
                )
            })
        })
        .collect()
}

fn bm25(profile: &Profile, weights: &BTreeMap<String, f64>, average_length: f64) -> f64 {
    weights
        .iter()
        .map(|(term, weight)| {
            let frequency = profile.frequencies.get(term).copied().unwrap_or(0.0);
            weight * frequency * 2.2
                / (frequency + 1.2 * (0.25 + 0.75 * profile.length / average_length))
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(id: usize, content: &str) -> HybridSearchResult {
        HybridSearchResult {
            id: id.to_string(),
            content: content.into(),
            user_id: "user".into(),
            app_id: "app".into(),
            level: 2,
            timestamp: 123,
            score: 0.42,
            bm25_score: 0.33,
            vector_score: 0.27,
            time_decay_factor: 0.9,
        }
    }

    fn ids(records: &[HybridSearchResult]) -> Vec<&str> {
        records.iter().map(|item| item.id.as_str()).collect()
    }

    #[test]
    fn promotes_query_evidence_and_preserves_every_field() {
        let input = vec![
            record(0, "protected unrelated"),
            record(1, "weather forecast"),
            record(2, "guitar budget is 400 euros"),
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
    fn distinct_coverage_beats_repeated_single_term() {
        let input = vec![
            record(0, "unrelated"),
            record(1, &"guitar ".repeat(100)),
            record(2, "guitar budget"),
        ];
        assert_eq!(ids(&select("guitar budget", input)), ["0", "2", "1"]);
    }

    #[test]
    fn density_beats_verbose_boilerplate() {
        let verbose = format!(
            "guitar budget {}",
            "ordinary surrounding prose ".repeat(100)
        );
        let input = vec![
            record(0, "protected"),
            record(1, &verbose),
            record(2, "guitar budget"),
        ];
        assert_eq!(ids(&select("guitar budget", input)), ["0", "2", "1"]);
    }

    #[test]
    fn equal_evidence_preserves_rank_and_first_is_protected() {
        let input = vec![
            record(0, "unrelated"),
            record(1, "guitar"),
            record(2, "guitar"),
        ];
        for _ in 0..3 {
            assert_eq!(ids(&select("guitar", input.clone())), ["0", "1", "2"]);
        }
    }

    #[test]
    fn unicode_and_case_normalize_without_format_parsing() {
        let input = vec![
            record(0, "protected"),
            record(1, "ordinary prose"),
            record(2, "ŻÓŁĆ 東京 ２０２６"),
        ];
        assert_eq!(ids(&select("żółć—東京 ２０２６", input)), ["0", "2", "1"]);
    }

    #[test]
    fn empty_absent_and_no_candidates_preserve_order() {
        for query in ["", "---", "unmatched"] {
            let input = vec![
                record(0, "protected"),
                record(1, "guitar"),
                record(2, "budget"),
            ];
            assert_eq!(ids(&select(query, input)), ["0", "1", "2"]);
        }
        assert!(select("query", Vec::new()).is_empty());
    }

    #[test]
    fn absent_query_terms_do_not_dilute_matching_terms() {
        let input = vec![
            record(0, "protected"),
            record(1, "unrelated"),
            record(2, "guitar"),
        ];
        assert_eq!(
            ids(&select("guitar absent", input.clone())),
            ids(&select("guitar", input))
        );
    }

    #[test]
    fn resource_limits_fail_open() {
        let distinct = (0..65)
            .map(|index| format!("term{index}"))
            .collect::<Vec<_>>()
            .join(" ");
        let trio = || {
            vec![
                record(0, "protected"),
                record(1, "other"),
                record(2, "guitar"),
            ]
        };
        // Each fixture would promote a tail record if its particular guard vanished.
        let filler = "x".repeat(MAX_RECORD_BYTES);
        let cases = vec![
            (
                "guitar".into(),
                (0..61)
                    .map(|index| record(index, if index == 60 { "guitar" } else { "other" }))
                    .collect(),
            ),
            ("guitar ".repeat(MAX_QUERY_BYTES / 7 + 1), trio()),
            (
                distinct,
                vec![
                    record(0, "protected"),
                    record(1, "other"),
                    record(2, "term0"),
                ],
            ),
            (
                "guitar".into(),
                vec![
                    record(0, "protected"),
                    record(1, &"x".repeat(MAX_RECORD_BYTES + 1)),
                    record(2, "guitar"),
                ],
            ),
            (
                "guitar".into(),
                (0..33)
                    .map(|index| record(index, if index == 32 { "guitar" } else { &filler }))
                    .collect(),
            ),
        ];
        for (query, input) in cases {
            let before = serde_json::to_value(&input).unwrap();
            assert_eq!(serde_json::to_value(select(&query, input)).unwrap(), before);
        }
    }
}
