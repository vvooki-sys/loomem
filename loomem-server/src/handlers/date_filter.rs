use chrono::{Datelike, NaiveDate, Utc};
use regex::Regex;
use std::sync::LazyLock;

use super::types::DateFilter;

static RE_ISO_DATE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(\d{4})-(\d{2})-(\d{2})").expect("ISO date regex is valid"));
static RE_DOTTED_DATE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(\d{1,2})\.(\d{2})\.(\d{4})").expect("dotted date regex is valid")
});
static RE_POLISH_DATE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(\d{1,2})\s+(stycznia|lutego|marca|kwietnia|maja|czerwca|lipca|sierpnia|września|października|listopada|grudnia)(?:\s+(\d{4}))?")
        .expect("Polish date regex is valid")
});

fn parse_polish_month(month_name: &str) -> Option<u32> {
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

    let now = Utc::now();
    let today_start = now
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .expect("valid static HMS values")
        .and_utc()
        .timestamp();
    let today_end = now
        .date_naive()
        .and_hms_opt(23, 59, 59)
        .expect("valid static HMS values")
        .and_utc()
        .timestamp();

    if query_lower.contains("dzisiaj") || query_lower.contains("dziś") {
        let cleaned = query_lower
            .replace("dzisiaj", "")
            .replace("dziś", "")
            .trim()
            .to_string();
        return (cleaned, Some(DateFilter::Range(today_start, today_end)));
    }

    if query_lower.contains("wczoraj") {
        let yesterday = now - chrono::Duration::days(1);
        let start = yesterday
            .date_naive()
            .and_hms_opt(0, 0, 0)
            .expect("valid static HMS values")
            .and_utc()
            .timestamp();
        let end = yesterday
            .date_naive()
            .and_hms_opt(23, 59, 59)
            .expect("valid static HMS values")
            .and_utc()
            .timestamp();
        let cleaned = query_lower.replace("wczoraj", "").trim().to_string();
        return (cleaned, Some(DateFilter::Range(start, end)));
    }

    if query_lower.contains("ostatni tydzień") || query_lower.contains("ostatni tydzie") {
        let week_ago = now - chrono::Duration::days(7);
        let start = week_ago.timestamp();
        let end = now.timestamp();
        let cleaned = query_lower
            .replace("ostatni tydzień", "")
            .replace("ostatni tydzie", "")
            .trim()
            .to_string();
        return (cleaned, Some(DateFilter::Range(start, end)));
    }

    if query_lower.contains("ostatni miesiąc") || query_lower.contains("ostatni miesią") {
        let month_ago = now - chrono::Duration::days(30);
        let start = month_ago.timestamp();
        let end = now.timestamp();
        let cleaned = query_lower
            .replace("ostatni miesiąc", "")
            .replace("ostatni miesią", "")
            .trim()
            .to_string();
        return (cleaned, Some(DateFilter::Range(start, end)));
    }

    // ISO date: YYYY-MM-DD
    if let Some(cap) = RE_ISO_DATE.captures(&query_lower) {
        if let (Ok(year), Ok(month), Ok(day)) = (
            cap[1].parse::<i32>(),
            cap[2].parse::<u32>(),
            cap[3].parse::<u32>(),
        ) {
            if let Some(date) = NaiveDate::from_ymd_opt(year, month, day) {
                let start = date
                    .and_hms_opt(0, 0, 0)
                    .expect("valid static HMS values")
                    .and_utc()
                    .timestamp();
                let end = date
                    .and_hms_opt(23, 59, 59)
                    .expect("valid static HMS values")
                    .and_utc()
                    .timestamp();
                let cleaned = RE_ISO_DATE.replace(&query_lower, "").trim().to_string();
                return (cleaned, Some(DateFilter::Range(start, end)));
            }
        }
    }

    // Dotted date: DD.MM.YYYY
    if let Some(cap) = RE_DOTTED_DATE.captures(&query_lower) {
        if let (Ok(day), Ok(month), Ok(year)) = (
            cap[1].parse::<u32>(),
            cap[2].parse::<u32>(),
            cap[3].parse::<i32>(),
        ) {
            if let Some(date) = NaiveDate::from_ymd_opt(year, month, day) {
                let start = date
                    .and_hms_opt(0, 0, 0)
                    .expect("valid static HMS values")
                    .and_utc()
                    .timestamp();
                let end = date
                    .and_hms_opt(23, 59, 59)
                    .expect("valid static HMS values")
                    .and_utc()
                    .timestamp();
                let cleaned = RE_DOTTED_DATE.replace(&query_lower, "").trim().to_string();
                return (cleaned, Some(DateFilter::Range(start, end)));
            }
        }
    }

    // Polish date: "DD MONTH" or "DD MONTH YYYY"
    if let Some(cap) = RE_POLISH_DATE.captures(&query_lower) {
        if let Ok(day) = cap[1].parse::<u32>() {
            if let Some(month) = parse_polish_month(&cap[2]) {
                let year = cap
                    .get(3)
                    .and_then(|m| m.as_str().parse::<i32>().ok())
                    .unwrap_or(now.year());

                if let Some(date) = NaiveDate::from_ymd_opt(year, month, day) {
                    let start = date
                        .and_hms_opt(0, 0, 0)
                        .expect("valid static HMS values")
                        .and_utc()
                        .timestamp();
                    let end = date
                        .and_hms_opt(23, 59, 59)
                        .expect("valid static HMS values")
                        .and_utc()
                        .timestamp();
                    let cleaned = RE_POLISH_DATE.replace(&query_lower, "").trim().to_string();
                    return (cleaned, Some(DateFilter::Range(start, end)));
                }
            }
        }
    }

    (query.to_string(), None)
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
            use chrono::DateTime;
            let date = DateTime::from_timestamp(start, 0).expect("start is a valid unix timestamp");
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
            use chrono::DateTime;
            let date = DateTime::from_timestamp(start, 0).expect("start is a valid unix timestamp");
            assert_eq!(date.day(), 23);
            assert_eq!(date.month(), 2);
            assert_eq!(date.year(), 2026);
        }
    }
}
