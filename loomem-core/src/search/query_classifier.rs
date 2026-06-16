//! Deterministic query classifier (cycle/85).
//!
//! Hard rule per arch §2 + §5.4: zero LLM call w hot path. Classifier is
//! pure regex + lightweight string scanning, sub-ms latency budget.
//!
//! Detection priority (first match wins):
//! 1. DocumentLookup — explicit upload/document verbs
//! 2. Relational — ≥2 proper nouns + relational preposition
//! 3. Temporal — date / cycle / temporal-marker regex
//! 4. Recent — recency marker without date
//! 5. Factual — fallback default
//!
//! Per brief §5: NER lite re-use of `EntityExtractor` was deferred —
//! /85 ships with regex-based proper noun detection (Unicode-aware,
//! supports PL+EN). Wiring `Arc<EntityExtractor>` into the classifier is
//! tracked as follow-up.

use regex::Regex;
use std::sync::OnceLock;

use super::query_taxonomy::{ClassifiedQuery, ParsedFeatures, QueryType, WeightVector};

/// Classify a query string into one of 5 query types and emit per-channel
/// weights + surface features. Synchronous, deterministic, zero allocations
/// beyond the returned `ClassifiedQuery` payload.
#[must_use]
pub fn classify(query: &str) -> ClassifiedQuery {
    let raw = RawSignals::parse(query);
    let query_type = pick_type(&raw);
    let weights = WeightVector::for_type(query_type);
    ClassifiedQuery {
        query_type,
        weights,
        features: raw.into_public(),
    }
}

/// Internal struct: public `ParsedFeatures` + intermediate signals (recent
/// markers, relational prepositions) consumed only by `pick_type`. The
/// intermediate signals don't go on the public surface — they're private
/// dispatch inputs, not user-facing artifacts.
struct RawSignals {
    entities: Vec<String>,
    temporal_markers: Vec<String>,
    doc_lookup_verbs: Vec<String>,
    language_hint: Option<String>,
    recent_hits: Vec<String>,
    relational_preposition_hits: Vec<String>,
}

impl RawSignals {
    fn parse(query: &str) -> Self {
        Self {
            entities: collect_proper_nouns(query),
            temporal_markers: collect_matches(query, temporal_regex()),
            doc_lookup_verbs: collect_matches(query, doc_lookup_regex()),
            language_hint: guess_language(query),
            recent_hits: collect_matches(query, recent_regex()),
            relational_preposition_hits: collect_matches(query, relational_prep_regex()),
        }
    }

    fn into_public(self) -> ParsedFeatures {
        ParsedFeatures {
            entities: self.entities,
            temporal_markers: self.temporal_markers,
            doc_lookup_verbs: self.doc_lookup_verbs,
            language_hint: self.language_hint,
        }
    }
}

/// Detection priority dispatch — returns the first matching type.
fn pick_type(raw: &RawSignals) -> QueryType {
    if !raw.doc_lookup_verbs.is_empty() {
        return QueryType::DocumentLookup;
    }
    if is_relational(raw) {
        return QueryType::Relational;
    }
    if !raw.temporal_markers.is_empty() {
        return QueryType::Temporal;
    }
    if !raw.recent_hits.is_empty() {
        return QueryType::Recent;
    }
    QueryType::Factual
}

fn is_relational(raw: &RawSignals) -> bool {
    raw.entities.len() >= 2 && !raw.relational_preposition_hits.is_empty()
}

fn collect_matches(query: &str, re: &Regex) -> Vec<String> {
    re.find_iter(query)
        .map(|m| m.as_str().to_string())
        .collect()
}

/// Capitalized tokens (Unicode-aware, includes PL diacritics): proper nouns
/// plus project codes like `BLAKE3`. Project-cycle codes `/NN` are not
/// entities — they're temporal markers, captured by `temporal_regex` instead.
fn collect_proper_nouns(query: &str) -> Vec<String> {
    let re = proper_noun_regex();
    re.find_iter(query)
        .map(|m| m.as_str().to_string())
        .filter(|s| !is_stopword(s))
        .collect()
}

/// Lowercase common-word filter: words that look proper-cased only because
/// they're sentence-initial. Conservative list to avoid false negatives.
fn is_stopword(token: &str) -> bool {
    matches!(
        token.to_lowercase().as_str(),
        "co" | "kto"
            | "gdzie"
            | "kiedy"
            | "jak"
            | "ile"
            | "który"
            | "która"
            | "które"
            | "what"
            | "who"
            | "where"
            | "when"
            | "how"
    )
}

fn guess_language(query: &str) -> Option<String> {
    let pl_diacritics = [
        'ą', 'ę', 'ż', 'ł', 'ó', 'ś', 'ć', 'ń', 'ź', 'Ą', 'Ę', 'Ż', 'Ł', 'Ó', 'Ś', 'Ć', 'Ń', 'Ź',
    ];
    if query.chars().any(|c| pl_diacritics.contains(&c)) {
        return Some("pl".to_string());
    }
    let lower = query.to_lowercase();
    let pl_markers = [
        " co ", " kto ", " gdzie ", " kiedy ", " ostatni", " wczoraj", "wgrał", "ten plik",
    ];
    let en_markers = [
        " what ",
        " who ",
        " where ",
        " when ",
        " uploaded",
        " yesterday",
        " recent",
    ];
    let pl_score = pl_markers.iter().filter(|m| lower.contains(*m)).count();
    let en_score = en_markers.iter().filter(|m| lower.contains(*m)).count();
    match pl_score.cmp(&en_score) {
        std::cmp::Ordering::Greater => Some("pl".to_string()),
        std::cmp::Ordering::Less => Some("en".to_string()),
        std::cmp::Ordering::Equal if pl_score > 0 || en_score > 0 => None,
        std::cmp::Ordering::Equal => None,
    }
}

// -----------------------------------------------------------------------------
// Regex tables (lazy-init via OnceLock — single allocation, no per-call cost).
// -----------------------------------------------------------------------------

fn temporal_regex() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(
            r"(?xi)
            \b(?:
                wczoraj | dziś | jutro | dzisiaj
                | w[\s]+(?:marcu|kwietniu|maju|czerwcu|lipcu|sierpniu|wrześniu|październiku|listopadzie|grudniu|styczniu|lutym)
                | yesterday | today | tomorrow
                | in[\s]+(?:january|february|march|april|may|june|july|august|september|october|november|december)
                | last[\s]+(?:week|month|year)
                | (?:miesiąc|tydzień|rok|day|days|week|month|year)\s+(?:temu|ago)
            )\b
            | /\d+
            | \b\d{4}\b
            ",
        )
        .expect("temporal regex must compile")
    })
}

fn recent_regex() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(
            r"(?xi)
            \b(?:
                ostatnia | ostatni | ostatnie | ostatnio | ostatnim | ostatnią
                | recent | recently | latest | last
                | nowy | nowa | nowe | nowości
                | świeży | świeże | świeżo | fresh | new
            )\b
            ",
        )
        .expect("recent regex must compile")
    })
}

fn doc_lookup_regex() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(
            r"(?xi)
            \b(?:
                wgrałem | wgrałam | wgraliśmy | wgrały | wgrał
                | załadowałem | załadowałam | załadowali
                | ten[\s]+plik | ten[\s]+paper | ten[\s]+dokument | ten[\s]+pdf
                | w[\s]+dokumencie | w[\s]+pdf | z[\s]+pdf | z[\s]+dokumentu
                | uploaded | upload
                | this[\s]+file | this[\s]+paper | this[\s]+document | this[\s]+pdf
                | in[\s]+pdf | in[\s]+document | in[\s]+the[\s]+document
            )\b
            ",
        )
        .expect("doc-lookup regex must compile")
    })
}

fn relational_prep_regex() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(
            r"(?xi)
            \b(?:
                z | razem[\s]+z | wraz[\s]+z | nad | dla | przeciwko
                | with | vs | versus | between | for | against | among
            )\b
            ",
        )
        .expect("relational-preposition regex must compile")
    })
}

fn proper_noun_regex() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        // Capitalized token: leading uppercase letter (PL+EN) + ≥1 alphanumeric.
        // `\p{Lu}` matches any Unicode uppercase letter; `\w` matches Unicode
        // word chars by default with the `unicode` feature of `regex`. Numbers
        // inside (e.g. BLAKE3, OAuth2) are intentional.
        Regex::new(r"\b\p{Lu}[\p{L}\p{N}_]*\b").expect("proper-noun regex must compile")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn types_of(queries: &[(&str, QueryType)]) {
        for (q, expected) in queries {
            let got = classify(q).query_type;
            assert_eq!(
                got, *expected,
                "query {q:?} expected {expected:?} got {got:?}"
            );
        }
    }

    #[test]
    fn test_detect_temporal_pl_wczoraj() {
        let c = classify("co było wczoraj");
        assert!(
            c.features.temporal_markers.iter().any(|m| m == "wczoraj"),
            "missing temporal marker, got {:?}",
            c.features.temporal_markers
        );
        assert_eq!(c.query_type, QueryType::Temporal);
    }

    #[test]
    fn test_detect_temporal_pl_cycle() {
        let c = classify("co w cyklu /50");
        assert!(c.features.temporal_markers.iter().any(|m| m == "/50"));
        assert_eq!(c.query_type, QueryType::Temporal);
    }

    #[test]
    fn test_detect_temporal_en_in_march() {
        let c = classify("what happened in march");
        assert!(c
            .features
            .temporal_markers
            .iter()
            .any(|m| m.contains("march")));
        assert_eq!(c.query_type, QueryType::Temporal);
    }

    #[test]
    fn test_detect_doc_lookup_pl_wgralem() {
        let c = classify("wgrałem mem0 paper");
        assert!(!c.features.doc_lookup_verbs.is_empty());
        assert_eq!(c.query_type, QueryType::DocumentLookup);
    }

    #[test]
    fn test_detect_doc_lookup_en_uploaded() {
        let c = classify("I uploaded this file yesterday");
        assert!(!c.features.doc_lookup_verbs.is_empty());
        assert_eq!(c.query_type, QueryType::DocumentLookup);
    }

    #[test]
    fn test_detect_recent_no_date() {
        let c = classify("ostatnia rzecz");
        assert_eq!(c.query_type, QueryType::Recent);
    }

    #[test]
    fn test_detect_recent_with_date_demoted_to_temporal() {
        // "ostatnio" + "w marcu" → temporal wins per priority dispatch.
        let c = classify("co było ostatnio w marcu");
        assert_eq!(c.query_type, QueryType::Temporal);
    }

    #[test]
    fn test_detect_relational_two_entities_with_preposition() {
        let c = classify("kto pracował z Mateuszem nad RBAC");
        assert!(c.features.entities.len() >= 2);
        assert_eq!(c.query_type, QueryType::Relational);
    }

    #[test]
    fn test_detect_relational_single_entity_no_match() {
        let c = classify("co Anna pisał");
        assert_eq!(c.features.entities.len(), 1);
        assert_eq!(c.query_type, QueryType::Factual);
    }

    #[test]
    fn test_fallback_factual() {
        let c = classify("co to jest BLAKE3");
        assert_eq!(c.query_type, QueryType::Factual);
    }

    #[test]
    fn test_classify_priority_doc_lookup_over_factual() {
        // doc-lookup verb wins even with proper noun present.
        let c = classify("wgrałem ten paper o Mem0");
        assert_eq!(c.query_type, QueryType::DocumentLookup);
    }

    #[test]
    fn test_classify_priority_temporal_over_recent() {
        // Both "ostatnio" and "w marcu" present → Temporal wins.
        let c = classify("co ostatnio w marcu");
        assert_eq!(c.query_type, QueryType::Temporal);
    }

    #[test]
    fn test_classify_emits_correct_weights_factual() {
        let c = classify("co to jest BLAKE3");
        assert_eq!(c.query_type, QueryType::Factual);
        // Factual: dense and lexical share the H tier (dominujące).
        assert!(c.weights.dense > c.weights.entity_match);
        assert!((c.weights.dense - c.weights.lexical).abs() < 1e-5);
    }

    #[test]
    fn test_classify_emits_correct_weights_document_lookup() {
        let c = classify("wgrałem ten paper");
        assert_eq!(c.query_type, QueryType::DocumentLookup);
        assert!(c.weights.dense > c.weights.entity_match);
        assert!(c.weights.dense > c.weights.recency);
        assert_eq!(c.weights.graph_edge, 0.0);
    }

    #[test]
    fn test_classify_features_surface_all_detected() {
        // Doc-lookup type wins, but features should expose all 3 detected
        // signal classes regardless of which type was selected.
        let c = classify("wgrałem mem0 paper o BLAKE3 w marcu");
        assert!(
            !c.features.doc_lookup_verbs.is_empty(),
            "doc verbs surfaced"
        );
        assert!(
            !c.features.temporal_markers.is_empty(),
            "temporal markers surfaced"
        );
        assert!(!c.features.entities.is_empty(), "entities surfaced");
    }

    #[test]
    fn test_classify_weights_sum_to_one_per_type() {
        for q in [
            "co to jest BLAKE3",
            "co było wczoraj",
            "kto pracował z Mateuszem nad RBAC",
            "ostatnia rzecz",
            "wgrałem ten paper",
        ] {
            let c = classify(q);
            let sum = c.weights.dense
                + c.weights.lexical
                + c.weights.entity_match
                + c.weights.graph_edge
                + c.weights.recency;
            assert!((sum - 1.0).abs() < 1e-5, "query {q:?} weights sum to {sum}");
        }
    }

    #[test]
    fn test_all_five_types_classifiable_via_fixtures() {
        types_of(&[
            ("co to jest BLAKE3", QueryType::Factual),
            ("co było wczoraj", QueryType::Temporal),
            ("kto pracował z Mateuszem nad RBAC", QueryType::Relational),
            ("ostatnia rzecz", QueryType::Recent),
            ("wgrałem ten paper o Mem0", QueryType::DocumentLookup),
        ]);
    }

    #[test]
    fn test_language_hint_pl_via_diacritics() {
        let c = classify("co Anna pisał");
        assert_eq!(c.features.language_hint.as_deref(), Some("pl"));
    }

    #[test]
    fn test_language_hint_en_via_markers() {
        let c = classify("what happened yesterday");
        assert_eq!(c.features.language_hint.as_deref(), Some("en"));
    }

    #[test]
    fn test_proper_noun_excludes_question_word() {
        // "Co" as sentence-initial question word should not count as entity.
        let c = classify("Co to jest BLAKE3");
        assert_eq!(c.features.entities.len(), 1);
        assert_eq!(c.features.entities[0], "BLAKE3");
    }
}
