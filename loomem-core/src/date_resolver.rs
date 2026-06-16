//! Deterministic date resolution: converts human-relative temporal references
//! (e.g. "yesterday", "last Friday", "3 dni temu") into absolute dates.
//!
//! Used by the consolidation pipeline to anchor events in time.
//! Pure Rust, zero LLM cost. Supports Polish and English.

use chrono::{Datelike, NaiveDate, Weekday};
use regex::Regex;
use std::sync::LazyLock;

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
        "monday" => Some(Weekday::Mon),
        "tuesday" => Some(Weekday::Tue),
        "wednesday" => Some(Weekday::Wed),
        "thursday" => Some(Weekday::Thu),
        "friday" => Some(Weekday::Fri),
        "saturday" => Some(Weekday::Sat),
        "sunday" => Some(Weekday::Sun),
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
        "poniedzialek" => Some(Weekday::Mon),
        "wtorek" => Some(Weekday::Tue),
        "srode" | "sroda" => Some(Weekday::Wed),
        "czwartek" => Some(Weekday::Thu),
        "piatek" => Some(Weekday::Fri),
        "sobote" | "sobota" => Some(Weekday::Sat),
        "niedziele" | "niedziela" => Some(Weekday::Sun),
        _ => None,
    }
}

/// Find the most recent past occurrence of a weekday, strictly before anchor.
/// If anchor IS that weekday, returns 7 days ago (previous week).
fn last_weekday(anchor: NaiveDate, target: Weekday) -> NaiveDate {
    let anchor_wd = anchor.weekday().num_days_from_monday();
    let target_wd = target.num_days_from_monday();
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
    anchor - chrono::Days::new(days_back as u64)
}

/// Resolve nearest past named month+day relative to anchor.
/// "March 15" on March 10 → previous year's March 15.
fn resolve_month_day(month: u32, day: u32, anchor: NaiveDate) -> Option<NaiveDate> {
    // Try current year first
    if let Some(date) = NaiveDate::from_ymd_opt(anchor.year(), month, day) {
        if date <= anchor {
            return Some(date);
        }
    }
    // Try previous year
    NaiveDate::from_ymd_opt(anchor.year() - 1, month, day)
}

// ── Public API ──────────────────────────────────────────────────────

/// Resolve a human-relative temporal reference to an absolute date.
///
/// Returns `None` for unrecognizable or fuzzy expressions ("recently", "soon").
///
/// # Arguments
/// - `raw`: verbatim temporal reference from text (e.g. "yesterday", "2026-03-15", "w zeszły piątek")
/// - `anchor`: reference date (typically the session/conversation date)
pub fn resolve_date(raw: &str, anchor: NaiveDate) -> Option<NaiveDate> {
    let normalized = raw.trim().to_lowercase();
    let s = normalized.as_str();

    // ── Absolute dates ──

    // ISO: 2026-03-15
    if let Some(caps) = RE_ISO.captures(s) {
        let y: i32 = caps[1].parse().ok()?;
        let m: u32 = caps[2].parse().ok()?;
        let d: u32 = caps[3].parse().ok()?;
        return NaiveDate::from_ymd_opt(y, m, d);
    }

    // Dotted: 15.03.2026
    if let Some(caps) = RE_DOTTED.captures(s) {
        let d: u32 = caps[1].parse().ok()?;
        let m: u32 = caps[2].parse().ok()?;
        let y: i32 = caps[3].parse().ok()?;
        return NaiveDate::from_ymd_opt(y, m, d);
    }

    // Slashed (LongMemEval): "2023/12/15" or "2023/12/15 (Fri) 14:30"
    if let Some(caps) = RE_SLASHED.captures(s) {
        let y: i32 = caps[1].parse().ok()?;
        let m: u32 = caps[2].parse().ok()?;
        let d: u32 = caps[3].parse().ok()?;
        return NaiveDate::from_ymd_opt(y, m, d);
    }

    // Polish named: "15 marca 2026" or "15 marca"
    if let Some(caps) = RE_POLISH_DATE.captures(s) {
        let d: u32 = caps[1].parse().ok()?;
        let m = parse_polish_month(&caps[2])?;
        if let Some(year_str) = caps.get(3) {
            let y: i32 = year_str.as_str().parse().ok()?;
            return NaiveDate::from_ymd_opt(y, m, d);
        }
        return resolve_month_day(m, d, anchor);
    }

    // English: "March 15, 2026" or "March 15"
    if let Some(caps) = RE_ENGLISH_DATE_ALT.captures(s) {
        let m = parse_english_month(&caps[1])?;
        let d: u32 = caps[2].parse().ok()?;
        if let Some(year_str) = caps.get(3) {
            let y: i32 = year_str.as_str().parse().ok()?;
            return NaiveDate::from_ymd_opt(y, m, d);
        }
        return resolve_month_day(m, d, anchor);
    }

    // ── Simple relative ──

    match s {
        "today" | "dziś" | "dzisiaj" | "dzis" => return Some(anchor),
        "yesterday" | "wczoraj" => return Some(anchor - chrono::Days::new(1)),
        "day before yesterday" | "przedwczoraj" => return Some(anchor - chrono::Days::new(2)),
        "tomorrow" | "jutro" => return Some(anchor + chrono::Days::new(1)),
        _ => {}
    }

    // "last week" / "w zeszłym tygodniu" → Monday of previous week
    if s == "last week" || s == "w zeszłym tygodniu" || s == "w zeszlym tygodniu" {
        return Some(
            last_weekday(anchor, Weekday::Mon)
                - chrono::Days::new(
                    last_weekday(anchor, Weekday::Mon)
                        .weekday()
                        .num_days_from_monday() as u64,
                ),
        );
    }

    // "last month" / "w zeszłym miesiącu"
    if s == "last month" || s == "w zeszłym miesiącu" || s == "w zeszlym miesiacu" {
        let prev = if anchor.month() == 1 {
            NaiveDate::from_ymd_opt(anchor.year() - 1, 12, 1)
        } else {
            NaiveDate::from_ymd_opt(anchor.year(), anchor.month() - 1, 1)
        };
        return prev;
    }

    // ── N units ago ──

    // English: "3 days ago"
    if let Some(caps) = RE_N_DAYS_AGO_EN.captures(s) {
        let n: u64 = caps[1].parse().ok()?;
        return Some(anchor - chrono::Days::new(n));
    }

    // English: "2 weeks ago"
    if let Some(caps) = RE_N_WEEKS_AGO_EN.captures(s) {
        let n: u64 = caps[1].parse().ok()?;
        return Some(anchor - chrono::Days::new(n * 7));
    }

    // English: "3 months ago"
    if let Some(caps) = RE_N_MONTHS_AGO_EN.captures(s) {
        let n: u32 = caps[1].parse().ok()?;
        return subtract_months(anchor, n);
    }

    // Polish: "3 dni temu"
    if let Some(caps) = RE_N_DNI_TEMU.captures(s) {
        let n: u64 = caps[1].parse().ok()?;
        return Some(anchor - chrono::Days::new(n));
    }

    // Polish: "2 tygodnie temu"
    if let Some(caps) = RE_N_TYGODNI_TEMU.captures(s) {
        let n: u64 = caps[1].parse().ok()?;
        return Some(anchor - chrono::Days::new(n * 7));
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
        return Some(last_weekday(anchor, wd));
    }

    // Polish: "w zeszły piątek"
    if let Some(caps) = RE_LAST_WEEKDAY_PL.captures(s) {
        let wd = parse_weekday_pl(&caps[1])?;
        return Some(last_weekday(anchor, wd));
    }

    // ── Unresolvable ──
    None
}

fn subtract_months(date: NaiveDate, months: u32) -> Option<NaiveDate> {
    let total_months = date.year() * 12 + date.month0() as i32 - months as i32;
    let y = total_months / 12;
    let m = (total_months % 12) as u32 + 1;
    let d = date.day().min(days_in_month(y, m));
    NaiveDate::from_ymd_opt(y, m, d)
}

fn days_in_month(year: i32, month: u32) -> u32 {
    NaiveDate::from_ymd_opt(year, month + 1, 1)
        .unwrap_or_else(|| NaiveDate::from_ymd_opt(year + 1, 1, 1).unwrap())
        .pred_opt()
        .unwrap()
        .day()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    // Anchor for most tests: 2026-04-05 (Sunday)
    #[allow(dead_code)] // used by future parameterized test helpers; kept as documented anchor
    const fn anchor() -> (i32, u32, u32) {
        (2026, 4, 5)
    }
    fn a() -> NaiveDate {
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
        let result = resolve_date("last week", a());
        assert!(result.is_some());
        // Should be Monday of previous week
        assert!(result.unwrap() < a());
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
