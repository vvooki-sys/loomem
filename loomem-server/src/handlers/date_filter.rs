use jiff::{civil::Date, SignedDuration, Timestamp};
use regex::Regex;
use std::sync::LazyLock;

use super::types::DateFilter;

pub(crate) fn now_unix_seconds() -> u64 {
    u64::from_le_bytes(Timestamp::now().as_second().to_le_bytes())
}

/// Preserve the legacy unsigned-to-signed Unix timestamp bit pattern.
pub(crate) fn unix_seconds_to_i64(seconds: u64) -> i64 {
    i64::from_le_bytes(seconds.to_le_bytes())
}

static RE_ISO_DATE: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r"\b(\d{4})-(\d{2})-(\d{2})\b").ok());
static RE_DOTTED_DATE: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r"\b(\d{1,2})\.(\d{2})\.(\d{4})\b").ok());
static RE_POLISH_DATE: LazyLock<Option<Regex>> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(\d{1,2})\s+(stycznia|lutego|marca|kwietnia|maja|czerwca|lipca|sierpnia|września|października|listopada|grudnia)(?:\s+(\d{4}))?\b").ok()
});
static RE_AGO: LazyLock<Option<Regex>> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(\d{1,3})\s+(day|days|week|weeks|month|months)\s+ago\b").ok()
});
static RE_TEMU: LazyLock<Option<Regex>> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(\d{1,3})\s+(dzień|dni|tydzień|tygodnie|miesiąc|miesiące)\s+temu\b").ok()
});
static RE_PAST: LazyLock<Option<Regex>> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:past|last)\s+(\d{1,3})\s+(day|days|week|weeks|month|months)\b").ok()
});
static RE_TODAY: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r"(?i)\b(?:dzisiaj|dziś|today)\b").ok());
static RE_YESTERDAY: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r"(?i)\b(?:wczoraj|yesterday)\b").ok());
static RE_LAST_WEEK: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r"(?i)\b(?:ostatni tydzień|last week)\b").ok());
static RE_LAST_MONTH: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r"(?i)\b(?:ostatni miesiąc|last month)\b").ok());
static LEGACY_RE_ISO_DATE: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r"(\d{4})-(\d{2})-(\d{2})").ok());
static LEGACY_RE_DOTTED_DATE: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r"(\d{1,2})\.(\d{2})\.(\d{4})").ok());
static LEGACY_RE_POLISH_DATE: LazyLock<Option<Regex>> = LazyLock::new(|| {
    Regex::new(r"(\d{1,2})\s+(stycznia|lutego|marca|kwietnia|maja|czerwca|lipca|sierpnia|września|października|listopada|grudnia)(?:\s+(\d{4}))?").ok()
});

fn parse_polish_month(month_name: &str) -> Option<i8> {
    match month_name.to_lowercase().as_str() {
        "stycznia" => Some(1),
        "lutego" => Some(2),
        "marca" => Some(3),
        "kwietnia" => Some(4),
        "maja" => Some(5),
        "czerwca" => Some(6),
        "lipca" => Some(7),
        "sierpnia" => Some(8),
        "września" => Some(9),
        "października" => Some(10),
        "listopada" => Some(11),
        "grudnia" => Some(12),
        _ => None,
    }
}

/// Extract date filter from query text.
/// Returns (cleaned query without date terms, optional date filter).
pub fn extract_date_filter(query: &str) -> (String, Option<DateFilter>) {
    let query_lower = query.to_lowercase();
    let now = crate::evaluation_clock::now();
    legacy_named_relative(&query_lower, now)
        .or_else(|| legacy_absolute_date(&query_lower, crate::temporal::utc_date(now).year()))
        .unwrap_or_else(|| (query.to_string(), None))
}

fn legacy_named_relative(query: &str, now: Timestamp) -> Option<(String, Option<DateFilter>)> {
    if query.contains("dzisiaj") || query.contains("dziś") {
        return legacy_today(query, now);
    }
    if query.contains("wczoraj") {
        return legacy_yesterday(query, now);
    }
    if query.contains("ostatni tydzień") || query.contains("ostatni tydzie") {
        return legacy_range(query, now, 7, "week");
    }
    if query.contains("ostatni miesiąc") || query.contains("ostatni miesią") {
        return legacy_range(query, now, 30, "month");
    }
    None
}

fn legacy_today(query: &str, now: Timestamp) -> Option<(String, Option<DateFilter>)> {
    let (start, end) = day_bounds(crate::temporal::utc_date(now))?;
    let cleaned = query
        .replace("dzisiaj", "")
        .replace("dziś", "")
        .trim()
        .to_string();
    Some((cleaned, Some(DateFilter::Range(start, end))))
}

fn legacy_yesterday(query: &str, now: Timestamp) -> Option<(String, Option<DateFilter>)> {
    let target = subtract_days(now, 1)?;
    let (start, end) = day_bounds(crate::temporal::utc_date(target))?;
    let cleaned = query.replace("wczoraj", "").trim().to_string();
    Some((cleaned, Some(DateFilter::Range(start, end))))
}

fn legacy_range(
    query: &str,
    now: Timestamp,
    days: i64,
    kind: &str,
) -> Option<(String, Option<DateFilter>)> {
    let start = subtract_days(now, days)?.as_second();
    let cleaned = if kind == "week" {
        query
            .replace("ostatni tydzień", "")
            .replace("ostatni tydzie", "")
    } else {
        query
            .replace("ostatni miesiąc", "")
            .replace("ostatni miesią", "")
    };
    Some((
        cleaned.trim().to_string(),
        Some(DateFilter::Range(start, now.as_second())),
    ))
}

fn legacy_absolute_date(query: &str, default_year: i16) -> Option<(String, Option<DateFilter>)> {
    parse_numeric_date(query, &LEGACY_RE_ISO_DATE, true, false)
        .or_else(|| parse_numeric_date(query, &LEGACY_RE_DOTTED_DATE, false, false))
        .or_else(|| parse_polish_with(query, default_year, &LEGACY_RE_POLISH_DATE, false))
}

/// Pure anchored equivalent used by deterministic replay requests.
pub fn extract_date_filter_at(
    query: &str,
    reference_time: Timestamp,
) -> (String, Option<DateFilter>) {
    let query_lower = query.to_lowercase();
    if anchored_marker_count(&query_lower) != 1 {
        return (query.to_string(), None);
    }
    if let Some(result) = relative_numeric(&query_lower, reference_time) {
        return result;
    }
    named_relative(&query_lower, reference_time)
        .or_else(|| {
            absolute_date(
                &query_lower,
                crate::temporal::utc_date(reference_time).year(),
            )
        })
        .unwrap_or_else(|| (query.to_string(), None))
}

fn anchored_marker_count(query: &str) -> usize {
    [
        &*RE_AGO,
        &*RE_TEMU,
        &*RE_PAST,
        &*RE_TODAY,
        &*RE_YESTERDAY,
        &*RE_LAST_WEEK,
        &*RE_LAST_MONTH,
        &*RE_ISO_DATE,
        &*RE_DOTTED_DATE,
        &*RE_POLISH_DATE,
    ]
    .into_iter()
    .filter_map(Option::as_ref)
    .map(|pattern| pattern.find_iter(query).count())
    .sum()
}

fn named_relative(query: &str, reference: Timestamp) -> Option<(String, Option<DateFilter>)> {
    if let Some(result) = matched_day(query, reference, &RE_TODAY, 0) {
        return Some(result);
    }
    if let Some(result) = matched_day(query, reference, &RE_YESTERDAY, 1) {
        return Some(result);
    }
    matched_range(query, reference, &RE_LAST_WEEK, 7)
        .or_else(|| matched_range(query, reference, &RE_LAST_MONTH, 30))
}

fn matched_day(
    query: &str,
    reference: Timestamp,
    pattern: &Option<Regex>,
    days_ago: i64,
) -> Option<(String, Option<DateFilter>)> {
    let pattern = pattern.as_ref()?;
    pattern.find(query)?;
    let target = subtract_days(reference, days_ago)?;
    let (start, end) = day_bounds(crate::temporal::utc_date(target))?;
    Some((clean(pattern, query), Some(DateFilter::Range(start, end))))
}

fn matched_range(
    query: &str,
    reference: Timestamp,
    pattern: &Option<Regex>,
    days: i64,
) -> Option<(String, Option<DateFilter>)> {
    let pattern = pattern.as_ref()?;
    pattern.find(query)?;
    let start = subtract_days(reference, days)?.as_second();
    Some((
        clean(pattern, query),
        Some(DateFilter::Range(start, reference.as_second())),
    ))
}

fn absolute_date(query: &str, default_year: i16) -> Option<(String, Option<DateFilter>)> {
    parse_numeric_date(query, &RE_ISO_DATE, true, true)
        .or_else(|| parse_numeric_date(query, &RE_DOTTED_DATE, false, true))
        .or_else(|| parse_polish_date(query, default_year))
}

fn parse_numeric_date(
    query: &str,
    pattern: &Option<Regex>,
    year_first: bool,
    all_matches: bool,
) -> Option<(String, Option<DateFilter>)> {
    let pattern = pattern.as_ref()?;
    let capture = pattern.captures(query)?;
    let (year, month, day) = numeric_parts(&capture, year_first)?;
    matched_date(
        query,
        pattern,
        Date::new(year, month, day).ok()?,
        all_matches,
    )
}

fn numeric_parts(capture: &regex::Captures<'_>, year_first: bool) -> Option<(i16, i8, i8)> {
    if year_first {
        parse_year_month_day(capture)
    } else {
        parse_day_month_year(capture)
    }
}

fn parse_year_month_day(capture: &regex::Captures<'_>) -> Option<(i16, i8, i8)> {
    let year = capture.get(1)?.as_str().parse().ok()?;
    let month = capture.get(2)?.as_str().parse().ok()?;
    let day = capture.get(3)?.as_str().parse().ok()?;
    Some((year, month, day))
}

fn parse_day_month_year(capture: &regex::Captures<'_>) -> Option<(i16, i8, i8)> {
    let day = capture.get(1)?.as_str().parse().ok()?;
    let month = capture.get(2)?.as_str().parse().ok()?;
    let year = capture.get(3)?.as_str().parse().ok()?;
    Some((year, month, day))
}

fn parse_polish_date(query: &str, default_year: i16) -> Option<(String, Option<DateFilter>)> {
    parse_polish_with(query, default_year, &RE_POLISH_DATE, true)
}

fn parse_polish_with(
    query: &str,
    default_year: i16,
    configured: &Option<Regex>,
    all_matches: bool,
) -> Option<(String, Option<DateFilter>)> {
    let pattern = configured.as_ref()?;
    let capture = pattern.captures(query)?;
    let day = capture.get(1)?.as_str().parse().ok()?;
    let month = parse_polish_month(capture.get(2)?.as_str())?;
    let year = capture
        .get(3)
        .and_then(|value| value.as_str().parse().ok())
        .unwrap_or(default_year);
    matched_date(
        query,
        pattern,
        Date::new(year, month, day).ok()?,
        all_matches,
    )
}

fn matched_date(
    query: &str,
    pattern: &Regex,
    date: Date,
    all_matches: bool,
) -> Option<(String, Option<DateFilter>)> {
    let (start, end) = day_bounds(date)?;
    let cleaned = if all_matches {
        clean(pattern, query)
    } else {
        pattern.replace(query, "").trim().to_string()
    };
    Some((cleaned, Some(DateFilter::Range(start, end))))
}

fn day_bounds(date: Date) -> Option<(i64, i64)> {
    let start = crate::temporal::utc_timestamp(date, 0)?;
    let end = crate::temporal::utc_timestamp(date, 23)? + 59 * 60 + 59;
    Some((start, end))
}

fn subtract_days(timestamp: Timestamp, days: i64) -> Option<Timestamp> {
    timestamp
        .checked_sub(SignedDuration::from_hours(days.checked_mul(24)?))
        .ok()
}

fn clean(pattern: &Regex, query: &str) -> String {
    pattern.replace_all(query, "").trim().to_string()
}

fn relative_numeric(query: &str, reference: Timestamp) -> Option<(String, Option<DateFilter>)> {
    if let Some(pattern) = RE_PAST.as_ref() {
        if let Some(captures) = pattern.captures(query) {
            return relative_range(query, reference, &captures, pattern);
        }
    }
    if let Some(pattern) = RE_AGO.as_ref() {
        if let Some(captures) = pattern.captures(query) {
            return relative_day(query, reference, &captures, pattern);
        }
    }
    let pattern = RE_TEMU.as_ref()?;
    pattern
        .captures(query)
        .and_then(|captures| relative_day(query, reference, &captures, pattern))
}

fn relative_day(
    query: &str,
    reference: Timestamp,
    captures: &regex::Captures<'_>,
    pattern: &Regex,
) -> Option<(String, Option<DateFilter>)> {
    let days = relative_days(captures)?;
    let target = subtract_days(reference, days)?;
    let (start, end) = day_bounds(crate::temporal::utc_date(target))?;
    let cleaned = clean(pattern, query);
    Some((cleaned, Some(DateFilter::Range(start, end))))
}

fn relative_range(
    query: &str,
    reference: Timestamp,
    captures: &regex::Captures<'_>,
    pattern: &Regex,
) -> Option<(String, Option<DateFilter>)> {
    let days = relative_days(captures)?;
    let start = subtract_days(reference, days)?.as_second();
    let cleaned = clean(pattern, query);
    Some((
        cleaned,
        Some(DateFilter::Range(start, reference.as_second())),
    ))
}

fn relative_days(captures: &regex::Captures<'_>) -> Option<i64> {
    let amount: i64 = captures.get(1)?.as_str().parse().ok()?;
    if !(1..=366).contains(&amount) {
        return None;
    }
    match captures.get(2)?.as_str().to_lowercase().as_str() {
        "day" | "days" | "dzień" | "dni" => Some(amount),
        "week" | "weeks" | "tydzień" | "tygodnie" => amount.checked_mul(7),
        "month" | "months" | "miesiąc" | "miesiące" => amount.checked_mul(30),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_date_extract_iso() {
        let (query, filter) = extract_date_filter("awaria 2026-02-23");
        assert_eq!(query, "awaria");
        assert!(filter.is_some());
        if let Some(DateFilter::Range(start, end)) = filter {
            assert_eq!(end - start, 86399);
        }
    }

    #[test]
    fn test_date_extract_polish() {
        let (query, filter) = extract_date_filter("co się stało 23 lutego 2026");
        assert_eq!(query, "co się stało");
        assert!(filter.is_some());
        if let Some(DateFilter::Range(start, end)) = filter {
            assert_eq!(end - start, 86399);
            let date = crate::temporal::utc_date(
                Timestamp::from_second(start).expect("start is a valid unix timestamp"),
            );
            assert_eq!(date.day(), 23);
            assert_eq!(date.month(), 2);
            assert_eq!(date.year(), 2026);
        }
    }

    #[test]
    fn test_date_extract_relative_yesterday() {
        let (query, filter) = extract_date_filter("wczoraj");
        assert_eq!(query, "");
        assert!(filter.is_some());
        if let Some(DateFilter::Range(start, end)) = filter {
            let duration = end - start;
            assert!((86399..=86400).contains(&duration));
        }
    }

    #[test]
    fn test_date_extract_range() {
        let (query, filter) = extract_date_filter("ostatni tydzień");
        assert_eq!(query, "");
        assert!(filter.is_some());
        if let Some(DateFilter::Range(start, end)) = filter {
            let duration = end - start;
            assert!((6 * 86400..=8 * 86400).contains(&duration));
        }
    }

    #[test]
    fn test_date_extract_none() {
        let (query, filter) = extract_date_filter("jaki font Acme");
        assert_eq!(query, "jaki font Acme");
        assert!(filter.is_none());
    }

    #[test]
    fn test_date_extract_dotted() {
        let (query, filter) = extract_date_filter("23.02.2026");
        assert_eq!(query, "");
        assert!(filter.is_some());
        if let Some(DateFilter::Range(start, end)) = filter {
            assert_eq!(end - start, 86399);
            let date = crate::temporal::utc_date(
                Timestamp::from_second(start).expect("start is a valid unix timestamp"),
            );
            assert_eq!(date.day(), 23);
            assert_eq!(date.month(), 2);
            assert_eq!(date.year(), 2026);
        }
    }

    fn anchor() -> Timestamp {
        Timestamp::from_second(1_792_411_200).unwrap()
    }

    #[test]
    fn anchored_english_and_polish_relative_dates_are_deterministic() {
        let english = extract_date_filter_at("activity 2 weeks ago", anchor());
        let polish = extract_date_filter_at("aktywność 2 tygodnie temu", anchor());
        assert_eq!(english.0, "activity");
        assert_eq!(polish.0, "aktywność");
        assert_eq!(english.1, polish.1);
        assert_eq!(
            english.1,
            Some(DateFilter::Range(1_791_158_400, 1_791_244_799))
        );
    }

    #[test]
    fn anchored_range_uses_reference_not_host_clock() {
        let (_, filter) = extract_date_filter_at("trips past 3 months", anchor());
        assert_eq!(
            filter,
            Some(DateFilter::Range(1_784_635_200, 1_792_411_200))
        );
    }

    #[test]
    fn out_of_range_relative_value_fails_open() {
        assert_eq!(
            extract_date_filter_at("activity 999 weeks ago", anchor()),
            ("activity 999 weeks ago".to_string(), None)
        );
    }

    #[test]
    fn marker_like_words_are_not_date_filters() {
        for query in [
            "last weekend plans",
            "last monthly report",
            "todayish ideas",
            "yesterdays notes",
        ] {
            assert_eq!(
                extract_date_filter_at(query, anchor()),
                (query.to_string(), None)
            );
        }
        for query in [
            "today and yesterday",
            "2026-02-23 then 2026-02-24",
            "2 weeks ago and 3 days ago",
        ] {
            assert_eq!(
                extract_date_filter_at(query, anchor()),
                (query.to_string(), None)
            );
        }
    }

    #[test]
    fn legacy_wrapper_preserves_pre_e1_substring_semantics() {
        let (week_query, week_filter) = extract_date_filter("plan ostatni tydzie");
        assert_eq!(week_query, "plan");
        assert!(week_filter.is_some());

        let (embedded_query, embedded_filter) = extract_date_filter("abc2026-02-23xyz");
        assert_eq!(embedded_query, "abcxyz");
        assert!(embedded_filter.is_some());

        let (two_dates, two_dates_filter) = extract_date_filter("2026-02-23 then 2026-02-24");
        assert_eq!(two_dates, "then 2026-02-24");
        assert!(two_dates_filter.is_some());

        let (polish_marker_query, polish_marker_filter) = extract_date_filter("dzisiajish");
        assert_eq!(polish_marker_query, "ish");
        assert!(polish_marker_filter.is_some());

        assert_eq!(extract_date_filter("today"), ("today".to_string(), None));
    }

    #[test]
    fn invalid_and_non_leap_dates_fail_open() {
        for query in ["event 2025-02-29", "event 31.12.10000", "event 0 marca"] {
            assert_eq!(
                extract_date_filter_at(query, anchor()),
                (query.to_string(), None)
            );
        }
    }

    #[test]
    fn leap_day_and_year_boundary_are_exact() {
        let leap = extract_date_filter_at("event 2024-02-29", anchor());
        let year = extract_date_filter_at(
            "event yesterday",
            Timestamp::from_second(1_767_227_400).unwrap(),
        );
        assert_eq!(
            leap.1,
            Some(DateFilter::Range(1_709_164_800, 1_709_251_199))
        );
        assert_eq!(
            year.1,
            Some(DateFilter::Range(1_767_139_200, 1_767_225_599))
        );
    }
}
