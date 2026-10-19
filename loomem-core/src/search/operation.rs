//! Deterministic, bounded operation facets for opt-in search experiments.
use chrono::{DateTime, Duration, Utc};
use regex::Regex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::sync::LazyLock;

pub const MAX_FACETS: usize = 2;
pub const MAX_ADDED_TERMS: usize = 6;
pub const MAX_TERM_CHARS: usize = 32;
pub const MAX_ADDED_BYTES: usize = 192;
pub const MAX_CANDIDATES: usize = 180;
pub const MAX_RETURNED: usize = 60;
pub const PROTECTED_RANKS: usize = 20;
pub const MAX_CHANGED_ADMISSIONS: usize = 2;
pub const MAX_RECORD_SCAN_BYTES: usize = 16 * 1024;
pub const MAX_REQUEST_SCAN_BYTES: usize = 512 * 1024;
pub const MAX_QUERY_BYTES: usize = 16 * 1024;

mod calendar;
pub use calendar::has_date_window_marker;
use calendar::last_month_interval;

static RELATIVE_REGEX: LazyLock<Option<Regex>> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(\d{1,3})\s+(day|days|week|weeks|month|months)\s+ago\b|\b(\d{1,3})\s+(dzień|dni|tydzień|tygodnie|miesiąc|miesiące)\s+temu\b",
    )
    .ok()
});
static COUNT_REGEX: LazyLock<Option<Regex>> = LazyLock::new(|| Regex::new(r"\b(\d{1,2})\b").ok());

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum OperationPolicy {
    #[default]
    #[serde(rename = "E0")]
    E0,
    #[serde(rename = "E1-D")]
    E1D,
    #[serde(rename = "E1-F")]
    E1F,
    #[serde(rename = "E1-C")]
    E1C,
    #[serde(rename = "E2-D")]
    E2D,
    #[serde(rename = "E2-F")]
    E2F,
    #[serde(rename = "E2-C")]
    E2C,
}

impl OperationPolicy {
    #[must_use]
    pub fn date_enabled(self) -> bool {
        matches!(self, Self::E1D | Self::E1C | Self::E2D | Self::E2C)
    }

    #[must_use]
    pub fn facets_enabled(self) -> bool {
        matches!(self, Self::E1F | Self::E1C | Self::E2F | Self::E2C)
    }

    pub fn is_e2(self) -> bool {
        matches!(self, Self::E2D | Self::E2F | Self::E2C)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct HalfOpenInterval {
    pub start: i64,
    pub end: i64,
}

impl HalfOpenInterval {
    pub fn inclusive_bounds(self) -> Option<(i64, i64)> {
        (self.start < self.end).then_some((self.start, self.end.checked_sub(1)?))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum QueryOperation {
    Plain,
    PreferenceAdvice {
        topic: String,
    },
    TemporalCompare {
        left_event: String,
        right_event: String,
    },
    TemporalEnumerate {
        topic: String,
        requested_count: Option<u8>,
    },
    TemporalWindow {
        topic: String,
        interval: (i64, i64),
        explicit_anchor: bool,
    },
    TemporalCalendarWindow {
        interval: HalfOpenInterval,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OperationPlan {
    pub operation: QueryOperation,
    pub facets: Vec<String>,
    pub added_terms: Vec<String>,
}

impl OperationPlan {
    #[must_use]
    pub fn plain() -> Self {
        Self {
            operation: QueryOperation::Plain,
            facets: Vec::new(),
            added_terms: Vec::new(),
        }
    }

    #[must_use]
    pub fn identity(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "plain".to_string())
    }

    #[must_use]
    pub fn identity_sha256(&self) -> String {
        format!("{:x}", Sha256::digest(self.identity().as_bytes()))
    }

    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self.operation {
            QueryOperation::Plain => "Plain",
            QueryOperation::PreferenceAdvice { .. } => "PreferenceAdvice",
            QueryOperation::TemporalCompare { .. } => "TemporalCompare",
            QueryOperation::TemporalEnumerate { .. } => "TemporalEnumerate",
            QueryOperation::TemporalWindow { .. } => "TemporalWindow",
            QueryOperation::TemporalCalendarWindow { .. } => "TemporalWindow",
        }
    }

    pub fn half_open_interval(&self) -> Option<HalfOpenInterval> {
        match self.operation {
            QueryOperation::TemporalCalendarWindow { interval, .. } => Some(interval),
            _ => None,
        }
    }

    #[must_use]
    pub fn expand(&self, existing: &str) -> String {
        let suffix = self.added_terms.join(" ");
        if suffix.is_empty() {
            existing.to_string()
        } else if existing.is_empty() {
            suffix
        } else {
            format!("{existing} {suffix}")
        }
    }
}

pub fn plan(
    query: &str,
    reference_time: Option<DateTime<Utc>>,
    policy: OperationPolicy,
) -> OperationPlan {
    if policy.is_e2() {
        return plan_e2(query, reference_time, policy);
    }
    if !policy.facets_enabled() || query.trim().is_empty() || query.len() > MAX_QUERY_BYTES {
        return OperationPlan::plain();
    }
    let lower = query.to_lowercase();
    let interval = reference_time.and_then(|reference| relative_interval(&lower, reference));
    let comparison = is_temporal_compare(&lower);
    let enumeration = is_temporal_enumerate(&lower);
    let preference = is_preference_advice(&lower);
    if match_count(interval, comparison, enumeration, preference) != 1 {
        return OperationPlan::plain();
    }
    if let Some(interval) = interval {
        return make_plan(
            QueryOperation::TemporalWindow {
                topic: topic(query),
                interval,
                explicit_anchor: true,
            },
            query,
            &["happened", "event", "date"],
        );
    }
    if comparison {
        let (left, right) = split_compare(query);
        return make_plan(
            QueryOperation::TemporalCompare {
                left_event: left,
                right_event: right,
            },
            query,
            &["before", "after", "earlier", "later", "happened", "date"],
        );
    }
    if enumeration {
        return make_plan(
            QueryOperation::TemporalEnumerate {
                topic: topic(query),
                requested_count: requested_count(&lower),
            },
            query,
            &["first", "next", "last", "visited", "went", "date"],
        );
    }
    if preference {
        return make_plan(
            QueryOperation::PreferenceAdvice {
                topic: topic(query),
            },
            query,
            &["prefer", "liked", "enjoy", "favorite", "dislike", "avoid"],
        );
    }
    OperationPlan::plain()
}

fn plan_e2(
    query: &str,
    reference_time: Option<DateTime<Utc>>,
    policy: OperationPolicy,
) -> OperationPlan {
    if query.trim().is_empty() || query.len() > MAX_QUERY_BYTES {
        return OperationPlan::plain();
    }
    let interval = reference_time
        .filter(|_| policy.date_enabled())
        .and_then(|anchor| last_month_interval(query, anchor));
    if let Some(interval) = interval {
        let operation = QueryOperation::TemporalCalendarWindow { interval };
        return if policy.facets_enabled() {
            make_plan(operation, query, &["happened", "event", "date"])
        } else {
            OperationPlan {
                operation,
                facets: Vec::new(),
                added_terms: Vec::new(),
            }
        };
    }
    if policy.facets_enabled() {
        plan(query, None, OperationPolicy::E1F)
    } else {
        OperationPlan::plain()
    }
}

fn match_count(
    interval: Option<(i64, i64)>,
    comparison: bool,
    enumeration: bool,
    preference: bool,
) -> usize {
    usize::from(interval.is_some())
        + usize::from(comparison)
        + usize::from(enumeration)
        + usize::from(preference)
}

fn make_plan(operation: QueryOperation, query: &str, markers: &[&str]) -> OperationPlan {
    let topic = topic(query);
    let facets = if topic.is_empty() {
        Vec::new()
    } else {
        vec![topic]
    };
    OperationPlan {
        operation,
        facets,
        added_terms: bounded_terms(query, markers),
    }
}

fn bounded_terms(query: &str, markers: &[&str]) -> Vec<String> {
    let mut bytes = 0;
    markers
        .iter()
        .map(|term| (*term).to_string())
        .filter(|term| !contains_token(query, term))
        .filter(|term| term.chars().count() <= MAX_TERM_CHARS)
        .filter(|term| {
            let next = bytes + term.len() + usize::from(bytes > 0);
            if next > MAX_ADDED_BYTES {
                false
            } else {
                bytes = next;
                true
            }
        })
        .take(MAX_ADDED_TERMS)
        .collect()
}

fn contains_token(query: &str, expected: &str) -> bool {
    query
        .split_whitespace()
        .map(clean_token)
        .any(|term| term == expected)
}

fn topic(query: &str) -> String {
    query
        .split_whitespace()
        .map(clean_token)
        .filter(|word| !word.is_empty() && !is_boilerplate(word))
        .take(16)
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(MAX_ADDED_BYTES)
        .collect()
}

fn clean_token(value: &str) -> String {
    value
        .trim_matches(|c: char| !c.is_alphanumeric() && c != '\'' && c != '-')
        .to_lowercase()
}

fn is_boilerplate(word: &str) -> bool {
    matches!(
        word,
        "what"
            | "which"
            | "how"
            | "could"
            | "would"
            | "should"
            | "please"
            | "tell"
            | "give"
            | "suggest"
            | "recommend"
            | "advice"
            | "tips"
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
            | "jaki"
            | "jak"
            | "co"
            | "który"
            | "proszę"
            | "poradź"
            | "poleć"
    )
}

fn is_preference_advice(query: &str) -> bool {
    contains_any_marker(
        query,
        &[
            "recommend",
            "recommendation",
            "recommendations",
            "suggest",
            "suggestion",
            "suggestions",
            "advice",
            "tips",
            "should i",
            "what should",
            "could i",
            "poleć",
            "poradź",
            "sugestia",
            "sugestie",
            "sugeruj",
        ],
    )
}

fn is_temporal_compare(query: &str) -> bool {
    contains_any_marker(
        query,
        &[
            "which happened first",
            "what happened first",
            "before or after",
            "which came first",
            "co było najpierw",
            "wcześniej czy później",
        ],
    )
}

fn is_temporal_enumerate(query: &str) -> bool {
    contains_any_marker(
        query,
        &[
            "chronological",
            "chronologically",
            "in order",
            "earliest to latest",
            "list all",
            "w kolejności",
            "chronologicznie",
            "chronologiczny",
            "chronologiczna",
        ],
    )
}

fn contains_any_marker(query: &str, markers: &[&str]) -> bool {
    markers.iter().any(|marker| contains_marker(query, marker))
}

fn contains_marker(query: &str, marker: &str) -> bool {
    query.match_indices(marker).any(|(start, _)| {
        let end = start + marker.len();
        let starts_at_boundary = query[..start]
            .chars()
            .next_back()
            .is_none_or(|value| !value.is_alphanumeric());
        let ends_at_boundary = query[end..]
            .chars()
            .next()
            .is_none_or(|value| !value.is_alphanumeric());
        starts_at_boundary && ends_at_boundary
    })
}

fn split_compare(query: &str) -> (String, String) {
    for separator in [" or ", " versus ", " vs ", " czy "] {
        if let Some((left, right)) = query.split_once(separator) {
            return (bounded_phrase(left), bounded_phrase(right));
        }
    }
    let phrase = bounded_phrase(query);
    (phrase.clone(), phrase)
}

fn bounded_phrase(value: &str) -> String {
    value.trim().chars().take(96).collect()
}

fn requested_count(query: &str) -> Option<u8> {
    count_regex()?
        .captures(query)
        .and_then(|capture| capture.get(1))
        .and_then(|value| value.as_str().parse().ok())
}

fn relative_interval(query: &str, reference: DateTime<Utc>) -> Option<(i64, i64)> {
    let pattern = relative_regex()?;
    if pattern.captures_iter(query).count() != 1 {
        return None;
    }
    let captures = pattern.captures(query)?;
    let (amount, unit) = relative_values(&captures)?;
    if !(1..=366).contains(&amount) {
        return None;
    }
    let days = relative_days(amount, &unit)?;
    day_interval(reference.checked_sub_signed(Duration::days(days))?)
}

fn relative_values(captures: &regex::Captures<'_>) -> Option<(i64, String)> {
    let amount = captures.get(1).or_else(|| captures.get(3))?;
    let unit = captures.get(2).or_else(|| captures.get(4))?;
    Some((amount.as_str().parse().ok()?, unit.as_str().to_string()))
}

fn relative_days(amount: i64, unit: &str) -> Option<i64> {
    let multiplier = match unit {
        "day" | "days" | "dzień" | "dni" => 1,
        "week" | "weeks" | "tydzień" | "tygodnie" => 7,
        "month" | "months" | "miesiąc" | "miesiące" => 30,
        _ => return None,
    };
    amount.checked_mul(multiplier)
}

fn day_interval(target: DateTime<Utc>) -> Option<(i64, i64)> {
    let start = target.date_naive().and_hms_opt(0, 0, 0)?.and_utc();
    let end = target.date_naive().and_hms_opt(23, 59, 59)?.and_utc();
    Some((start.timestamp(), end.timestamp()))
}

fn relative_regex() -> Option<&'static Regex> {
    RELATIVE_REGEX.as_ref()
}

fn count_regex() -> Option<&'static Regex> {
    COUNT_REGEX.as_ref()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn anchor() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 19, 12, 0, 0).unwrap()
    }

    #[test]
    fn policies_register_exact_ablation_matrix() {
        assert_eq!(
            serde_json::to_string(&OperationPolicy::E0).unwrap(),
            "\"E0\""
        );
        assert!(OperationPolicy::E1D.date_enabled());
        assert!(!OperationPolicy::E1D.facets_enabled());
        assert!(OperationPolicy::E1F.facets_enabled());
        assert!(!OperationPolicy::E1F.date_enabled());
        assert!(OperationPolicy::E1C.date_enabled());
        assert!(OperationPolicy::E1C.facets_enabled());
    }

    #[test]
    fn plan_identity_hash_is_canonical_and_binds_facets() {
        let plain = OperationPlan::plain();
        assert_eq!(plain.kind(), "Plain");
        assert_eq!(
            plain.identity_sha256(),
            "7a9b7d8895a49f8abe56070490fdb1d9e84dc2df8606b9144dc773da6dfb4c45"
        );
        let mut changed = plain;
        changed.facets.push("private derived facet".into());
        assert_ne!(changed.identity_sha256(), changed.identity());
        assert_ne!(
            changed.identity_sha256(),
            OperationPlan::plain().identity_sha256()
        );
        assert_eq!(changed.identity_sha256().len(), 64);
    }

    #[test]
    fn preference_plan_is_bounded_and_repetition_independent() {
        let one = plan("Could you suggest guitar tips?", None, OperationPolicy::E1F);
        let repeated = plan(
            &"Could you suggest guitar tips? ".repeat(100),
            None,
            OperationPolicy::E1F,
        );
        assert!(matches!(
            one.operation,
            QueryOperation::PreferenceAdvice { .. }
        ));
        assert_eq!(one.added_terms, repeated.added_terms);
        assert!(one.added_terms.len() <= MAX_ADDED_TERMS);
        assert!(one
            .added_terms
            .iter()
            .all(|term| term.chars().count() <= MAX_TERM_CHARS));
        assert!(one.added_terms.join(" ").len() <= MAX_ADDED_BYTES);
    }

    #[test]
    fn explicit_relative_window_is_deterministic() {
        let plan = plan(
            "What happened 2 weeks ago?",
            Some(anchor()),
            OperationPolicy::E1C,
        );
        let QueryOperation::TemporalWindow {
            interval,
            explicit_anchor,
            ..
        } = plan.operation
        else {
            panic!("expected window")
        };
        assert!(explicit_anchor);
        assert_eq!(interval, (1_791_158_400, 1_791_244_799));
    }

    #[test]
    fn missing_anchor_and_plain_queries_fail_open() {
        assert_eq!(
            plan("What happened 2 weeks ago?", None, OperationPolicy::E1C),
            OperationPlan::plain()
        );
        assert_eq!(
            plan(
                "Where is the invoice?",
                Some(anchor()),
                OperationPolicy::E1C
            ),
            OperationPlan::plain()
        );
        assert_eq!(
            plan("Could you suggest guitar tips?", None, OperationPolicy::E0),
            OperationPlan::plain()
        );
    }

    #[test]
    fn oversized_and_ambiguous_relative_queries_are_plain() {
        assert_eq!(
            plan(
                &format!("Could you suggest {}?", "x".repeat(MAX_QUERY_BYTES)),
                None,
                OperationPolicy::E1C,
            ),
            OperationPlan::plain()
        );
        assert_eq!(
            plan(
                "What happened 2 weeks ago and 3 days ago?",
                Some(anchor()),
                OperationPolicy::E1C,
            ),
            OperationPlan::plain()
        );
    }

    #[test]
    fn all_registered_operation_variants_work_in_english_and_polish() {
        for query in ["Recommend a guitar", "Poleć gitarę"] {
            assert!(matches!(
                plan(query, None, OperationPolicy::E1F).operation,
                QueryOperation::PreferenceAdvice { .. }
            ));
        }
        for query in [
            "Which happened first, launch or move?",
            "Co było najpierw, start czy meta?",
        ] {
            assert!(matches!(
                plan(query, None, OperationPolicy::E1F).operation,
                QueryOperation::TemporalCompare { .. }
            ));
        }
        for query in [
            "List all 3 trips in chronological order",
            "Wymień 4 podróże w kolejności",
        ] {
            assert!(matches!(
                plan(query, None, OperationPolicy::E1F).operation,
                QueryOperation::TemporalEnumerate { .. }
            ));
        }
        for query in ["What happened 2 weeks ago?", "Co było 2 tygodnie temu?"] {
            assert!(matches!(
                plan(query, Some(anchor()), OperationPolicy::E1C).operation,
                QueryOperation::TemporalWindow { .. }
            ));
        }
    }

    #[test]
    fn parser_preserves_concrete_phrases_negation_and_requested_count() {
        let compare = plan(
            "Which happened first, launch or move?",
            None,
            OperationPolicy::E1F,
        );
        let QueryOperation::TemporalCompare {
            left_event,
            right_event,
        } = compare.operation
        else {
            panic!("expected comparison")
        };
        assert!(left_event.contains("launch"));
        assert!(right_event.contains("move"));

        let enumerate = plan(
            "List all 3 trips in chronological order",
            None,
            OperationPolicy::E1F,
        );
        assert!(matches!(
            enumerate.operation,
            QueryOperation::TemporalEnumerate {
                requested_count: Some(3),
                ..
            }
        ));
        let preference = plan(
            "What should I not buy for podróż?",
            None,
            OperationPolicy::E1F,
        );
        let QueryOperation::PreferenceAdvice { topic } = preference.operation else {
            panic!("expected preference")
        };
        assert!(topic.contains("not buy"));
        assert!(topic.contains("podróż"));
    }

    #[test]
    fn boundaries_and_conflicting_shapes_fail_open() {
        for query in [
            "tipsy guitar",
            "suggestionsish guitar",
            "recommend trips in chronological order",
            "Which happened first 2 weeks ago?",
        ] {
            assert_eq!(
                plan(query, Some(anchor()), OperationPolicy::E1C),
                OperationPlan::plain(),
                "{query}"
            );
        }
        let max_count = plan(
            "List all 99 trips in chronological order",
            None,
            OperationPolicy::E1F,
        );
        assert!(matches!(
            max_count.operation,
            QueryOperation::TemporalEnumerate {
                requested_count: Some(99),
                ..
            }
        ));
        assert!(max_count.facets.len() <= MAX_FACETS);
        assert!(max_count
            .added_terms
            .iter()
            .all(|term| term.chars().count() <= MAX_TERM_CHARS));
        assert!(max_count.added_terms.join(" ").len() <= MAX_ADDED_BYTES);
    }

    #[test]
    fn gold_like_suffix_does_not_change_query_derived_plan() {
        let query = "Which happened first, buying a kettle or repairing it?";
        assert_eq!(
            plan(query, None, OperationPolicy::E1F),
            plan(query, None, OperationPolicy::E1F)
        );
    }
}
