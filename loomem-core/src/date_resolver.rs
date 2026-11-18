//! Deterministic date resolution: converts human-relative temporal references
//! (e.g. "yesterday", "last Friday", "3 dni temu") into absolute dates.
//!
//! Used by the consolidation pipeline to anchor events in time.
//! Pure Rust, zero LLM cost. Supports Polish and English.

use jiff::civil::{Date, Weekday};
use jiff::{SignedDuration, Span, Timestamp};
use regex::Regex;
use std::sync::LazyLock;

fn date_from_ymd(year: impl Into<i32>, month: u32, day: u32) -> Option<Date> {
    Date::new(
        i16::try_from(year.into()).ok()?,
        i8::try_from(month).ok()?,
        i8::try_from(day).ok()?,
    )
    .ok()
}

/// Convert an unsigned Unix timestamp to a UTC date without truncating it.
pub(crate) fn date_from_unix(timestamp: u64) -> Option<Date> {
    i64::try_from(timestamp)
        .ok()
        .and_then(|timestamp| Timestamp::from_second(timestamp).ok())
        .map(crate::temporal::utc_date)
}

pub(crate) fn format_now_utc(format: &str) -> String {
    Timestamp::now()
        .to_zoned_datetime(jiff::tz::TimeZone::UTC)
        .strftime(format)
        .to_string()
}

pub(crate) fn today_iso() -> String {
    format_now_utc("%Y-%m-%d")
}

fn subtract_days(date: Date, days: i64) -> Option<Date> {
    let hours = days.checked_mul(24)?;
    date.checked_sub(SignedDuration::from_hours(hours)).ok()
}

fn add_days(date: Date, days: i64) -> Option<Date> {
    let hours = days.checked_mul(24)?;
    date.checked_add(SignedDuration::from_hours(hours)).ok()
}

// ── Absolute date patterns ──────────────────────────────────────────

static RE_ISO: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(\d{4})-(\d{2})-(\d{2})$").unwrap());

static RE_DOTTED: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(\d{1,2})\.(\d{1,2})\.(\d{4})$").unwrap());

// LongMemEval format: "2023/12/15" or "2023/12/15 (Fri) 14:30"
static RE_SLASHED: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(\d{4})/(\d{1,2})/(\d{1,2})\b").unwrap());

static RE_POLISH_DATE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(\d{1,2})\s+(stycznia|lutego|marca|kwietnia|maja|czerwca|lipca|sierpnia|wrze[śs]nia|pa[źz]dziernika|listopada|grudnia)(?:\s+(\d{4}))?$").unwrap()
});

static RE_ENGLISH_DATE_ALT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(january|february|march|april|may|june|july|august|september|october|november|december)\s+(\d{1,2})(?:\s*,?\s*(\d{4}))?$").unwrap()
});

// ── Relative date patterns ──────────────────────────────────────────

static RE_N_DAYS_AGO_EN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(\d+)\s+days?\s+ago$").unwrap());

static RE_N_WEEKS_AGO_EN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(\d+)\s+weeks?\s+ago$").unwrap());

static RE_N_MONTHS_AGO_EN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(\d+)\s+months?\s+ago$").unwrap());

static RE_N_DNI_TEMU: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(\d+)\s+(?:dzień|dnia|dni)\s+temu$").unwrap());

static RE_N_TYGODNI_TEMU: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(\d+)\s+(?:tygodnie|tygodnia|tygodni|tydzień)\s+temu$").unwrap()
});

// Polish `ą` (miesiąc = 1, miesiące = 2-4), `ę` (miesięcy = 5+), `e` (colloquial miesiec).
static RE_N_MIESIECY_TEMU: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(\d+)\s+miesi[ąęe]c(?:y|e|a|)\s+temu$").unwrap());

static RE_LAST_WEEKDAY_EN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^last\s+(monday|tuesday|wednesday|thursday|friday|saturday|sunday)$").unwrap()
});

static RE_LAST_WEEKDAY_PL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^w\s+zesz[łl][yą]\s+(poniedzia[łl]ek|wtorek|[śs]rod[ęe]|czwartek|pi[ąa]tek|sobot[ęe]|niedziel[ęe])$").unwrap()
});

// ── Month name lookups ──────────────────────────────────────────────

fn parse_polish_month(name: &str) -> Option<u32> {
    match name {
        "stycznia" => Some(1),
        "lutego" => Some(2),
        "marca" => Some(3),
        "kwietnia" => Some(4),
        "maja" => Some(5),
        "czerwca" => Some(6),
        "lipca" => Some(7),
        "sierpnia" => Some(8),
        "września" | "wrzesnia" => Some(9),
        "października" | "pazdziernika" => Some(10),
        "listopada" => Some(11),
        "grudnia" => Some(12),
        _ => None,
    }
}

fn parse_english_month(name: &str) -> Option<u32> {
    match name {
        "january" => Some(1),
        "february" => Some(2),
        "march" => Some(3),
        "april" => Some(4),
        "may" => Some(5),
        "june" => Some(6),
        "july" => Some(7),
        "august" => Some(8),
        "september" => Some(9),
        "october" => Some(10),
        "november" => Some(11),
        "december" => Some(12),
        _ => None,
    }
}

fn parse_weekday_en(name: &str) -> Option<Weekday> {
    match name {
        "monday" => Some(Weekday::Monday),
        "tuesday" => Some(Weekday::Tuesday),
        "wednesday" => Some(Weekday::Wednesday),
        "thursday" => Some(Weekday::Thursday),
        "friday" => Some(Weekday::Friday),
        "saturday" => Some(Weekday::Saturday),
        "sunday" => Some(Weekday::Sunday),
        _ => None,
    }
}

fn parse_weekday_pl(name: &str) -> Option<Weekday> {
    let normalized = name
        .replace('ł', "l")
        .replace('ś', "s")
        .replace('ą', "a")
        .replace('ę', "e");
    match normalized.as_str() {
        "poniedzialek" => Some(Weekday::Monday),
        "wtorek" => Some(Weekday::Tuesday),
        "srode" | "sroda" => Some(Weekday::Wednesday),
        "czwartek" => Some(Weekday::Thursday),
        "piatek" => Some(Weekday::Friday),
        "sobote" | "sobota" => Some(Weekday::Saturday),
        "niedziele" | "niedziela" => Some(Weekday::Sunday),
        _ => None,
    }
}

/// Find the most recent past occurrence of a weekday, strictly before anchor.
/// If anchor IS that weekday, returns 7 days ago (previous week).
fn last_weekday(anchor: Date, target: Weekday) -> Option<Date> {
    let anchor_wd = i64::from(anchor.weekday().to_monday_zero_offset());
    let target_wd = i64::from(target.to_monday_zero_offset());
    let days_back = if anchor_wd >= target_wd {
        let diff = anchor_wd - target_wd;
        if diff == 0 {
            7
        } else {
            diff
        }
    } else {
        7 - (target_wd - anchor_wd)
    };
    subtract_days(anchor, days_back)
}

/// Resolve nearest past named month+day relative to anchor.
/// "March 15" on March 10 → previous year's March 15.
fn resolve_month_day(month: u32, day: u32, anchor: Date) -> Option<Date> {
    // Try current year first
    if let Some(date) = date_from_ymd(anchor.year(), month, day) {
        if date <= anchor {
            return Some(date);
        }
    }
    // Try previous year
    date_from_ymd(anchor.year().checked_sub(1)?, month, day)
}

fn resolve_ymd(s: &str, regex: &Regex, order: [usize; 3]) -> Option<Option<Date>> {
    let caps = regex.captures(s)?;
    let year: i32 = caps.get(order[0])?.as_str().parse().ok()?;
    let month: u32 = caps.get(order[1])?.as_str().parse().ok()?;
    let day: u32 = caps.get(order[2])?.as_str().parse().ok()?;
    Some(date_from_ymd(year, month, day))
}

fn resolve_named_date(
    caps: regex::Captures<'_>,
    month_parser: fn(&str) -> Option<u32>,
    month_index: usize,
    day_index: usize,
    year_index: usize,
    anchor: Date,
) -> Option<Date> {
    let month = month_parser(caps.get(month_index)?.as_str())?;
    let day: u32 = caps.get(day_index)?.as_str().parse().ok()?;
    if let Some(year) = caps.get(year_index) {
        let year: i32 = year.as_str().parse().ok()?;
        return date_from_ymd(year, month, day);
    }
    resolve_month_day(month, day, anchor)
}

fn resolve_absolute_date(s: &str, anchor: Date) -> Option<Option<Date>> {
    resolve_ymd(s, &RE_ISO, [1, 2, 3])
        .or_else(|| resolve_ymd(s, &RE_DOTTED, [3, 2, 1]))
        .or_else(|| resolve_ymd(s, &RE_SLASHED, [1, 2, 3]))
        .or_else(|| {
            RE_POLISH_DATE
                .captures(s)
                .map(|caps| resolve_named_date(caps, parse_polish_month, 2, 1, 3, anchor))
        })
        .or_else(|| {
            RE_ENGLISH_DATE_ALT
                .captures(s)
                .map(|caps| resolve_named_date(caps, parse_english_month, 1, 2, 3, anchor))
        })
}

// ── Public API ──────────────────────────────────────────────────────

/// Resolve a human-relative temporal reference to an absolute date.
///
/// Returns `None` for unrecognizable or fuzzy expressions ("recently", "soon").
///
/// # Arguments
/// - `raw`: verbatim temporal reference from text (e.g. "yesterday", "2026-03-15", "w zeszły piątek")
/// - `anchor`: reference date (typically the session/conversation date)
pub fn resolve_date(raw: &str, anchor: Date) -> Option<Date> {
    let normalized = raw.trim().to_lowercase();
    let s = normalized.as_str();

    if let Some(date) = resolve_absolute_date(s, anchor) {
        return date;
    }

    // ── Simple relative ──

    match s {
        "today" | "dziś" | "dzisiaj" | "dzis" => return Some(anchor),
        "yesterday" | "wczoraj" => return subtract_days(anchor, 1),
        "day before yesterday" | "przedwczoraj" => return subtract_days(anchor, 2),
        "tomorrow" | "jutro" => return add_days(anchor, 1),
        _ => {}
    }

    // "last week" / "w zeszłym tygodniu" → Monday of previous week
    if s == "last week" || s == "w zeszłym tygodniu" || s == "w zeszlym tygodniu" {
        let days_back = i64::from(anchor.weekday().to_monday_zero_offset()) + 7;
        return subtract_days(anchor, days_back);
    }

    // "last month" / "w zeszłym miesiącu"
    if s == "last month" || s == "w zeszłym miesiącu" || s == "w zeszlym miesiacu" {
        let month = u32::try_from(anchor.month()).ok()?;
        if month == 1 {
            return date_from_ymd(anchor.year().checked_sub(1)?, 12, 1);
        }
        return date_from_ymd(anchor.year(), month - 1, 1);
    }

    // ── N units ago ──

    // English: "3 days ago"
    if let Some(caps) = RE_N_DAYS_AGO_EN.captures(s) {
        let n: u64 = caps[1].parse().ok()?;
        return subtract_days(anchor, i64::try_from(n).ok()?);
    }

    // English: "2 weeks ago"
    if let Some(caps) = RE_N_WEEKS_AGO_EN.captures(s) {
        let n: u64 = caps[1].parse().ok()?;
        let days = i64::try_from(n).ok()?.checked_mul(7)?;
        return subtract_days(anchor, days);
    }

    // English: "3 months ago"
    if let Some(caps) = RE_N_MONTHS_AGO_EN.captures(s) {
        let n: u32 = caps[1].parse().ok()?;
        return subtract_months(anchor, n);
    }

    // Polish: "3 dni temu"
    if let Some(caps) = RE_N_DNI_TEMU.captures(s) {
        let n: u64 = caps[1].parse().ok()?;
        return subtract_days(anchor, i64::try_from(n).ok()?);
    }

    // Polish: "2 tygodnie temu"
    if let Some(caps) = RE_N_TYGODNI_TEMU.captures(s) {
        let n: u64 = caps[1].parse().ok()?;
        let days = i64::try_from(n).ok()?.checked_mul(7)?;
        return subtract_days(anchor, days);
    }

    // Polish: "3 miesiące temu"
    if let Some(caps) = RE_N_MIESIECY_TEMU.captures(s) {
        let n: u32 = caps[1].parse().ok()?;
        return subtract_months(anchor, n);
    }

    // ── Last [weekday] ──

    // English: "last Friday"
    if let Some(caps) = RE_LAST_WEEKDAY_EN.captures(s) {
        let wd = parse_weekday_en(&caps[1])?;
        return last_weekday(anchor, wd);
    }

    // Polish: "w zeszły piątek"
    if let Some(caps) = RE_LAST_WEEKDAY_PL.captures(s) {
        let wd = parse_weekday_pl(&caps[1])?;
        return last_weekday(anchor, wd);
    }

    // ── Unresolvable ──
    None
}

fn subtract_months(date: Date, months: u32) -> Option<Date> {
    let months = i32::try_from(months).ok()?;
    date.checked_sub(Span::new().months(months)).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(y: i32, m: u32, d: u32) -> Date {
        date_from_ymd(y, m, d).unwrap()
    }

    // Anchor for most tests: 2026-04-05 (Sunday)
    #[allow(dead_code)] // used by future parameterized test helpers; kept as documented anchor
    const fn anchor() -> (i32, u32, u32) {
        (2026, 4, 5)
    }
    fn a() -> Date {
        d(2026, 4, 5)
    }

    // ── Absolute dates ──

    #[test]
    fn iso_date() {
        assert_eq!(resolve_date("2026-03-15", a()), Some(d(2026, 3, 15)));
    }

    #[test]
    fn iso_date_invalid() {
        assert_eq!(resolve_date("2026-02-31", a()), None);
    }

    #[test]
    fn dotted_date() {
        assert_eq!(resolve_date("15.03.2026", a()), Some(d(2026, 3, 15)));
    }

    #[test]
    fn slashed_date_lme() {
        // LongMemEval format
        assert_eq!(resolve_date("2023/12/15", a()), Some(d(2023, 12, 15)));
        assert_eq!(resolve_date("2024/3/5", a()), Some(d(2024, 3, 5)));
    }

    #[test]
    fn slashed_date_with_weekday_time() {
        // LongMemEval timestamps include weekday + time
        assert_eq!(
            resolve_date("2023/12/15 (Fri) 14:30", a()),
            Some(d(2023, 12, 15))
        );
    }

    #[test]
    fn polish_date_with_year() {
        assert_eq!(resolve_date("15 marca 2026", a()), Some(d(2026, 3, 15)));
    }

    #[test]
    fn polish_date_without_year_past() {
        // March 15 is before April 5 → this year
        assert_eq!(resolve_date("15 marca", a()), Some(d(2026, 3, 15)));
    }

    #[test]
    fn polish_date_without_year_future() {
        // May 15 is after April 5 → previous year
        assert_eq!(resolve_date("15 maja", a()), Some(d(2025, 5, 15)));
    }

    #[test]
    fn english_date_with_year() {
        assert_eq!(resolve_date("March 15, 2026", a()), Some(d(2026, 3, 15)));
    }

    #[test]
    fn english_date_without_year() {
        assert_eq!(resolve_date("March 15", a()), Some(d(2026, 3, 15)));
    }

    #[test]
    fn english_date_future_month() {
        assert_eq!(resolve_date("June 10", a()), Some(d(2025, 6, 10)));
    }

    // ── Simple relative ──

    #[test]
    fn today_en() {
        assert_eq!(resolve_date("today", a()), Some(a()));
    }

    #[test]
    fn today_pl() {
        assert_eq!(resolve_date("dzisiaj", a()), Some(a()));
        assert_eq!(resolve_date("dziś", a()), Some(a()));
    }

    #[test]
    fn yesterday_en() {
        assert_eq!(resolve_date("yesterday", a()), Some(d(2026, 4, 4)));
    }

    #[test]
    fn yesterday_pl() {
        assert_eq!(resolve_date("wczoraj", a()), Some(d(2026, 4, 4)));
    }

    #[test]
    fn day_before_yesterday() {
        assert_eq!(resolve_date("przedwczoraj", a()), Some(d(2026, 4, 3)));
        assert_eq!(
            resolve_date("day before yesterday", a()),
            Some(d(2026, 4, 3))
        );
    }

    #[test]
    fn tomorrow() {
        assert_eq!(resolve_date("tomorrow", a()), Some(d(2026, 4, 6)));
        assert_eq!(resolve_date("jutro", a()), Some(d(2026, 4, 6)));
    }

    // ── N units ago ──

    #[test]
    fn n_days_ago_en() {
        assert_eq!(resolve_date("3 days ago", a()), Some(d(2026, 4, 2)));
        assert_eq!(resolve_date("1 day ago", a()), Some(d(2026, 4, 4)));
    }

    #[test]
    fn n_days_ago_pl() {
        // nom.sg. (1 dzień) — previously missed by `dn(?:i|ia|)` root.
        assert_eq!(resolve_date("1 dzień temu", a()), Some(d(2026, 4, 4)));
        // nom.pl. 2+ (3 dni).
        assert_eq!(resolve_date("3 dni temu", a()), Some(d(2026, 4, 2)));
        // gen.sg. (1 dnia) — preserved from previous regex.
        assert_eq!(resolve_date("1 dnia temu", a()), Some(d(2026, 4, 4)));
    }

    #[test]
    fn n_weeks_ago_en() {
        assert_eq!(resolve_date("2 weeks ago", a()), Some(d(2026, 3, 22)));
    }

    #[test]
    fn n_weeks_ago_pl() {
        // nom.sg. (1 tydzień) — previously missed by `tygodn...` root.
        assert_eq!(resolve_date("1 tydzień temu", a()), Some(d(2026, 3, 29)));
        // nom.pl. 2-4 (2 tygodnie).
        assert_eq!(resolve_date("2 tygodnie temu", a()), Some(d(2026, 3, 22)));
        // gen.pl. 5+ (5 tygodni) — longest-first alternation avoids prefix match.
        assert_eq!(resolve_date("5 tygodni temu", a()), Some(d(2026, 3, 1)));
    }

    #[test]
    fn n_months_ago_en() {
        assert_eq!(resolve_date("3 months ago", a()), Some(d(2026, 1, 5)));
    }

    #[test]
    fn n_months_ago_pl() {
        // ą forms (miesiąc = 1, miesiące = 2-4) — previously missed by `miesi[ęe]c`.
        assert_eq!(resolve_date("1 miesiąc temu", a()), Some(d(2026, 3, 5)));
        assert_eq!(resolve_date("3 miesiące temu", a()), Some(d(2026, 1, 5)));
        // ę form (miesięcy = 5+).
        assert_eq!(resolve_date("5 miesięcy temu", a()), Some(d(2025, 11, 5)));
        // Colloquial ascii fallback (miesiec / miesiecy).
        assert_eq!(resolve_date("2 miesiec temu", a()), Some(d(2026, 2, 5)));
    }

    #[test]
    fn relative_dates_preserve_leap_days_and_clamp_month_ends() {
        assert_eq!(
            resolve_date("3 months ago", d(2026, 5, 31)),
            Some(d(2026, 2, 28))
        );
        assert_eq!(
            resolve_date("3 miesiące temu", d(2024, 5, 31)),
            Some(d(2024, 2, 29))
        );
        assert_eq!(
            resolve_date("yesterday", d(2026, 1, 1)),
            Some(d(2025, 12, 31))
        );
        assert_eq!(resolve_date("2024-02-29", a()), Some(d(2024, 2, 29)));
        assert_eq!(resolve_date("2026-02-29", a()), None);
    }

    // ── Last [weekday] ──

    #[test]
    fn last_friday_en() {
        // April 5, 2026 is Sunday. Last Friday = April 3
        assert_eq!(resolve_date("last Friday", a()), Some(d(2026, 4, 3)));
    }

    #[test]
    fn last_sunday_on_sunday() {
        // Anchor IS Sunday. "Last Sunday" = 7 days ago, not today
        assert_eq!(resolve_date("last Sunday", a()), Some(d(2026, 3, 29)));
    }

    #[test]
    fn last_monday_en() {
        // April 5 is Sunday. Last Monday = March 31
        assert_eq!(resolve_date("last Monday", a()), Some(d(2026, 3, 30)));
    }

    #[test]
    fn last_friday_pl() {
        assert_eq!(resolve_date("w zeszły piątek", a()), Some(d(2026, 4, 3)));
    }

    #[test]
    fn last_monday_pl() {
        assert_eq!(
            resolve_date("w zeszły poniedziałek", a()),
            Some(d(2026, 3, 30))
        );
    }

    // ── Last week / month ──

    #[test]
    fn last_week_en() {
        assert_eq!(resolve_date("last week", a()), Some(d(2026, 3, 23)));
    }

    #[test]
    fn last_week_all_weekdays_and_languages() {
        for day in 5..=11 {
            for expression in ["last week", "w zeszłym tygodniu", "w zeszlym tygodniu"] {
                assert_eq!(
                    resolve_date(expression, d(2026, 10, day)),
                    Some(d(2026, 9, 28)),
                    "expression={expression}, anchor day={day}"
                );
            }
        }
    }

    #[test]
    fn last_week_crosses_year_boundary() {
        assert_eq!(
            resolve_date("last week", d(2026, 1, 1)),
            Some(d(2025, 12, 22))
        );
    }

    #[test]
    fn last_week_outside_calendar_returns_none() {
        assert_eq!(resolve_date("last week", Date::MIN), None);
    }

    #[test]
    fn last_month_en() {
        assert_eq!(resolve_date("last month", a()), Some(d(2026, 3, 1)));
    }

    #[test]
    fn last_month_january() {
        // Edge: last month from January → December of previous year
        let jan = d(2026, 1, 15);
        assert_eq!(resolve_date("last month", jan), Some(d(2025, 12, 1)));
    }

    // ── Unresolvable ──

    #[test]
    fn unresolvable() {
        assert_eq!(resolve_date("recently", a()), None);
        assert_eq!(resolve_date("niedawno", a()), None);
        assert_eq!(resolve_date("soon", a()), None);
        assert_eq!(resolve_date("wkrótce", a()), None);
        assert_eq!(resolve_date("a while ago", a()), None);
        assert_eq!(resolve_date("some random text", a()), None);
        assert_eq!(resolve_date("", a()), None);
    }

    // ── Whitespace / case ──

    #[test]
    fn case_insensitive() {
        assert_eq!(resolve_date("YESTERDAY", a()), Some(d(2026, 4, 4)));
        assert_eq!(resolve_date("Last Friday", a()), Some(d(2026, 4, 3)));
    }

    #[test]
    fn whitespace_trimming() {
        assert_eq!(resolve_date("  yesterday  ", a()), Some(d(2026, 4, 4)));
    }
}
