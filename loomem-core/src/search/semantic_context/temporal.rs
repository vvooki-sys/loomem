//! Conservative lexical safety veto for transient evidence projection.
//! This is not a complete semantic classifier: false positives retain E0.

/// Keep original evidence for explicit chronology, recency, or state-change intent.
/// Unicode alphanumeric words are lowercased; matching is whole-word only.
#[must_use]
pub fn requires_original_order(query: &str) -> bool {
    if query.len() > crate::search::context_evidence::MAX_QUERY_BYTES {
        return true;
    }
    let words: Vec<_> = query
        .split(|character: char| !character.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_lowercase)
        .collect();
    words.iter().any(|word| temporal_word(word))
        || words.windows(2).any(|pair| {
            matches!(
                (pair[0].as_str(), pair[1].as_str()),
                ("no", "longer") | ("as", "of")
            )
        })
        || !crate::search::query_classifier::classify(query)
            .features
            .temporal_markers
            .is_empty()
}

fn temporal_word(word: &str) -> bool {
    matches!(
        word,
        "when"
            | "before"
            | "after"
            | "since"
            | "until"
            | "earlier"
            | "later"
            | "first"
            | "last"
            | "earliest"
            | "latest"
            | "newest"
            | "recent"
            | "recently"
            | "now"
            | "current"
            | "currently"
            | "previous"
            | "previously"
            | "former"
            | "formerly"
            | "still"
            | "anymore"
            | "chronology"
            | "chronological"
            | "chronologically"
            | "timeline"
            | "history"
            | "historical"
            | "date"
            | "dates"
            | "change"
            | "changes"
            | "changed"
            | "changing"
            | "update"
            | "updates"
            | "updated"
            | "updating"
            | "switch"
            | "switches"
            | "switched"
            | "switching"
            | "became"
            | "become"
            | "started"
            | "stopped"
            | "kiedy"
            | "przed"
            | "po"
            | "odkąd"
            | "wcześniej"
            | "później"
            | "pierwszy"
            | "pierwsza"
            | "pierwsze"
            | "ostatni"
            | "ostatnia"
            | "ostatnie"
            | "ostatnio"
            | "najnowszy"
            | "najnowsza"
            | "najnowsze"
            | "teraz"
            | "obecnie"
            | "aktualnie"
            | "aktualny"
            | "aktualna"
            | "aktualne"
            | "poprzedni"
            | "poprzednia"
            | "poprzednie"
            | "nadal"
            | "już"
            | "chronologia"
            | "chronologicznie"
            | "zmiana"
            | "zmiany"
            | "zmienił"
            | "zmieniła"
            | "zmieniłem"
            | "zmieniłam"
            | "zmieniło"
            | "zmieniły"
            | "zaktualizował"
            | "zaktualizowała"
            | "rozpoczął"
            | "rozpoczęła"
            | "przestał"
            | "przestała"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_temporal_operations_veto_projection() {
        for query in [
            "Which office is CURRENT?",
            "When did I move?",
            "Has the schedule changed?",
            "What came before the workshop?",
            "Does that option still apply?",
            "What applied as-of the meeting?",
            "I no longer use it; what replaced it?",
            "Compare Elm and Ash in March",
            "Compare Elm and Ash for 2031",
            "Która opcja jest aktualna?",
            "Co zmieniłam?",
            "Jak było wcześniej?",
            "Stan po spotkaniu",
            "Wybierz pierwszy pomysł",
            "Show cycle /42 context",
        ] {
            assert!(requires_original_order(query), "{query}");
        }
    }

    #[test]
    fn oversized_query_vetoes_before_scanning_and_relational_priority_does_not_hide_dates() {
        assert!(requires_original_order(&"x".repeat(16 * 1024 + 1)));
        let query = "Compare Elm versus Ash in March";
        assert_eq!(
            crate::search::query_classifier::classify(query).query_type,
            crate::search::query_taxonomy::QueryType::Relational
        );
        assert!(requires_original_order(query));
    }

    #[test]
    fn static_preference_and_advice_queries_do_not_match_substrings() {
        for query in [
            "Recommend activities matching my interests",
            "What constraints suit my goals?",
            "Which workshop fits my experience?",
            "Porównaj pomysły zgodne z moimi pasjami",
            "Discuss currency exchange and datefruit recipes",
            "Explain aftermarket supplies",
            "Suggest a firstclass cabin",
            "Summarize noteworthy preferences",
        ] {
            assert!(!requires_original_order(query), "{query}");
        }
    }
}
