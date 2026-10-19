//! Canonical calendar-window recognition shared by planning and lane safety gates.
use super::HalfOpenInterval;
use chrono::{DateTime, Datelike, Utc};
use regex::Regex;
use std::sync::LazyLock;

static DATE_WINDOW_REGEX: LazyLock<Option<Regex>> = LazyLock::new(|| {
    Regex::new(concat!(
        r"(?i)\b(?:today|yesterday|tomorrow|(?:last|this|next)\s+(?:day|week|month|year)",
        r"|(?:past|last)\s+\d+\s+(?:days?|weeks?|months?|years?)",
        r"|\d+\s+(?:days?|weeks?|months?|years?)\s+ago",
        r"|\d{4}-\d{1,2}-\d{1,2}|\d{1,2}[./]\d{1,2}[./]\d{2,4}|\d{4}",
        r"|january|february|march|april|june|july|august|september|october|november|december",
        r"|may\s+\d{1,2}|\d{1,2}\s+may|(?:in|during|since|before|after|until|between)\s+may",
        r"|stycznia|lutego|marca|kwietnia|maja|czerwca|lipca|sierpnia|września|października|listopada|grudnia",
        r"|dziś|dzisiaj|wczoraj|jutro|ostatni\s+(?:dzień|tydzień|miesiąc|rok)",
        r"|(?:zeszł[ymyaei]+|ubiegł[ymyaei]+|przyszł[ymyaei]+)\s+(?:dni[au]|tygodni[au]|miesiąc[au]|rok[uu]?)",
        r"|\d+\s+(?:dzień|dni|tydzień|tygodnie|tygodni|miesiąc|miesiące|miesięcy|lat|lata)\s+temu)\b"
    ))
    .ok()
});

/// Whether a request contains a date window that must resolve before combined retrieval.
/// A parser initialization error conservatively requires date resolution.
pub fn has_date_window_marker(query: &str) -> bool {
    DATE_WINDOW_REGEX
        .as_ref()
        .is_none_or(|pattern| pattern.is_match(query))
}

pub(super) fn last_month_interval(
    query: &str,
    reference: DateTime<Utc>,
) -> Option<HalfOpenInterval> {
    let mut markers = DATE_WINDOW_REGEX.as_ref()?.find_iter(query);
    let marker = markers.next()?.as_str().to_lowercase();
    (markers.next().is_none() && matches!(marker.as_str(), "last month" | "ostatni miesiąc"))
        .then_some(())?;
    let end_date = reference.date_naive().with_day(1)?;
    let start_date = end_date.pred_opt()?.with_day(1)?;
    let end = end_date.and_hms_opt(0, 0, 0)?.and_utc().timestamp();
    Some(HalfOpenInterval {
        start: start_date.and_hms_opt(0, 0, 0)?.and_utc().timestamp(),
        end,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn date_markers_are_bounded_and_do_not_capture_facet_only_operations() {
        for query in [
            "recommend yesterday",
            "list all trips last month",
            "2 weeks ago",
            "3 dni temu",
            "recommend next year",
            "wczoraj",
            "recommend on 2023-04-01",
            "recommend in 2023",
            "recommend on 12.04.2023",
            "recommend on April 12",
            "recommend on May 12",
            "recommend in May",
            "list all trips past 2 weeks",
            "Poleć gitarę w zeszłym tygodniu",
        ] {
            assert!(has_date_window_marker(query), "{query}");
        }
        for query in [
            "Recommend a guitar",
            "List all trips in order",
            "Which happened first, launch or move?",
            "yesterdaysong",
        ] {
            assert!(!has_date_window_marker(query), "{query}");
        }
    }
}
