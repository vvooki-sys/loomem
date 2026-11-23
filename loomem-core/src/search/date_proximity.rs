//! Event-date proximity boost (`[search] event_date_proximity_boost`).
//!
//! A query with a date range (`date_from`/`date_to`, or a date parsed out of
//! the query text) admits chunks by date but, until this lever, ranked them
//! as if the range did not exist. [`multiplier`] turns the distance between a
//! chunk's event date and the range into a score multiplier.
//!
//! Granularity is the whole UTC day: event dates are stored as calendar days
//! (UTC midnight) while the handler's ranges end at `23:59:59`, so both are
//! reduced to day numbers before measuring. With `n` = number of days in the
//! range and `d` = whole days from the event day to the nearer range day:
//!
//! - inside the range (`d = 0`, edges included): `1.0 + weight`;
//! - outside: `1.0 + weight * (1 - d / (falloff + 1))`, where
//!   `falloff = min(n, MAX_FALLOFF_DAYS)` — linear decay that reaches
//!   `1.0` at `falloff + 1` days away. The day just before and the day just
//!   after a range get the same partial boost (a single-day range gives both
//!   neighbours `1.0 + weight / 2`). The cap keeps open-ended ranges
//!   (`date_to` alone is `0..=end`, `date_from` alone runs to now) from
//!   flattening the decay over decades;
//! - no event time, no range, or a weight that is not a positive finite
//!   number: exactly `1.0`.
//!
//! With the shipped default `weight = 0.0` the multiplier is exactly `1.0` on
//! every branch, and `score * 1.0 == score` in IEEE 754, so ranking is
//! unchanged. The function is pure; the caller decides where the event time
//! and the range come from.

/// Seconds in a UTC day.
const DAY_SECS: i64 = 86_400;

/// Upper bound, in days, on how far outside the range the boost decays.
pub const MAX_FALLOFF_DAYS: i64 = 365;

/// Whether `weight` turns the boost on: a positive finite number. Callers
/// can use it to skip loading the event time when the lever is off.
#[must_use]
pub fn enabled(weight: f64) -> bool {
    weight.is_finite() && weight > 0.0
}

/// Score multiplier for a chunk whose event happened at `event_ts` (unix
/// seconds), given the query range `range = (start, end)` (unix seconds,
/// inclusive) and the configured `weight`. Both are compared by UTC day.
///
/// Returns exactly `1.0` when `weight` is not a positive finite number, when
/// `event_ts` or `range` is `None`, or when the event day lies more than the
/// fall-off (see the module docs) outside the range. A reversed range
/// (`start > end`) is treated as its normalised form.
#[must_use]
pub fn multiplier(event_ts: Option<i64>, range: Option<(i64, i64)>, weight: f64) -> f64 {
    if !enabled(weight) {
        return 1.0;
    }
    let (Some(event), Some((a, b))) = (event_ts, range) else {
        return 1.0;
    };
    // Day numbers of i64 seconds stay within ±1.1e14, so the day
    // differences below cannot overflow.
    let event = event.div_euclid(DAY_SECS);
    let (first, last) = (a.min(b).div_euclid(DAY_SECS), a.max(b).div_euclid(DAY_SECS));
    let distance = if event < first {
        first - event
    } else if event > last {
        event - last
    } else {
        return 1.0 + weight;
    };
    let falloff = (last - first + 1).min(MAX_FALLOFF_DAYS);
    if distance > falloff {
        return 1.0;
    }
    1.0 + weight * remaining_fraction(falloff + 1 - distance, falloff + 1)
}

/// `remaining / total` for `1 <= remaining < total <= MAX_FALLOFF_DAYS + 1`;
/// both fit in `u32`, so the division is exact up to f64 rounding.
fn remaining_fraction(remaining: i64, total: i64) -> f64 {
    match (u32::try_from(remaining), u32::try_from(total)) {
        (Ok(remaining), Ok(total)) => f64::from(remaining) / f64::from(total),
        _ => 0.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: i64 = DAY_SECS;
    /// 2024-01-01T00:00:00Z ..= 2024-12-31T23:59:59Z (366-day leap year),
    /// the shape the handler builds from `date_from`/`date_to`.
    const Y2024: (i64, i64) = (1_704_067_200, 1_735_689_599);
    /// 2024-06-01 as the handler builds a single day: `(d, d + 86_399)`.
    const JUNE1: i64 = 1_717_200_000;
    const ONE_DAY: (i64, i64) = (JUNE1, JUNE1 + DAY - 1);

    fn approx(actual: f64, expected: f64) -> bool {
        (actual - expected).abs() < 1e-12
    }

    /// Every branch of [`multiplier`] for one weight.
    fn cases() -> Vec<(&'static str, Option<i64>, Option<(i64, i64)>)> {
        let (start, end) = Y2024;
        vec![
            ("no event", None, Some(Y2024)),
            ("no range", Some(start), None),
            ("neither", None, None),
            ("inside", Some(start + 150 * DAY), Some(Y2024)),
            ("start edge", Some(start), Some(Y2024)),
            ("end edge", Some(end), Some(Y2024)),
            ("day before", Some(start - DAY), Some(Y2024)),
            ("day after", Some(end + 1), Some(Y2024)),
            ("half falloff after", Some(end + 1 + 182 * DAY), Some(Y2024)),
            ("past falloff", Some(end + 1 + 400 * DAY), Some(Y2024)),
            ("far before", Some(0), Some(Y2024)),
            ("far after", Some(i64::MAX), Some(Y2024)),
            ("i64::MIN", Some(i64::MIN), Some(Y2024)),
            (
                "reversed range inside",
                Some(start + DAY),
                Some((end, start)),
            ),
            ("single day neighbour", Some(JUNE1 + DAY), Some(ONE_DAY)),
            ("open-ended range", Some(end + 1), Some((0, end))),
        ]
    }

    #[test]
    fn zero_weight_is_exactly_one_on_every_branch() {
        for (name, event, range) in cases() {
            let m = multiplier(event, range, 0.0);
            assert_eq!(m.to_bits(), 1.0_f64.to_bits(), "{name}");
        }
    }

    #[test]
    fn non_positive_or_non_finite_weight_is_exactly_one() {
        for weight in [-0.0, -1.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            for (name, event, range) in cases() {
                let m = multiplier(event, range, weight);
                assert_eq!(m.to_bits(), 1.0_f64.to_bits(), "{name} weight={weight}");
            }
        }
    }

    #[test]
    fn multiplying_by_the_neutral_multiplier_keeps_score_bits() {
        for score in [0.0, -0.0, 1e-300, 0.123_456_789, 3.5, f64::MAX] {
            let boosted = score * multiplier(Some(Y2024.0), Some(Y2024), 0.0);
            assert_eq!(boosted.to_bits(), score.to_bits(), "score={score}");
        }
    }

    #[test]
    fn enabled_only_for_positive_finite_weights() {
        assert!(enabled(0.25));
        assert!(enabled(1.0));
        for weight in [0.0, -0.0, -1.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert!(!enabled(weight), "weight={weight}");
        }
    }

    #[test]
    fn missing_event_or_range_is_neutral() {
        assert_eq!(multiplier(None, Some(Y2024), 1.0), 1.0);
        assert_eq!(multiplier(Some(Y2024.0), None, 1.0), 1.0);
        assert_eq!(multiplier(None, None, 1.0), 1.0);
    }

    #[test]
    fn inside_and_on_edges_get_the_full_boost() {
        let (start, end) = Y2024;
        assert_eq!(multiplier(Some(start + 150 * DAY), Some(Y2024), 1.0), 2.0);
        assert_eq!(multiplier(Some(start), Some(Y2024), 0.5), 1.5);
        // An event dated on the last day is stored at its midnight.
        assert_eq!(multiplier(Some(end + 1 - DAY), Some(Y2024), 0.5), 1.5);
        assert_eq!(multiplier(Some(end), Some(Y2024), 0.5), 1.5);
        assert_eq!(multiplier(Some(start + DAY), Some((end, start)), 1.0), 2.0);
        assert_eq!(multiplier(Some(JUNE1), Some(ONE_DAY), 1.0), 2.0);
    }

    #[test]
    fn single_day_range_neighbours_get_equal_partial_boost() {
        let before = multiplier(Some(JUNE1 - DAY), Some(ONE_DAY), 1.0);
        let after = multiplier(Some(JUNE1 + DAY), Some(ONE_DAY), 1.0);
        assert_eq!(before, after);
        assert!(before > 1.0 && before < 2.0, "{before}");
        assert_eq!(before, 1.5);
        assert_eq!(multiplier(Some(JUNE1 + 2 * DAY), Some(ONE_DAY), 1.0), 1.0);
        assert_eq!(multiplier(Some(JUNE1 - 2 * DAY), Some(ONE_DAY), 1.0), 1.0);
    }

    #[test]
    fn neighbours_of_a_year_range_are_symmetric() {
        let (start, end) = Y2024;
        for days in [1, 15, 100, 365] {
            let before = multiplier(Some(start - days * DAY), Some(Y2024), 1.0);
            let after = multiplier(Some(end + 1 + (days - 1) * DAY), Some(Y2024), 1.0);
            assert_eq!(before, after, "days={days}");
            assert!(before > 1.0 && before < 2.0, "days={days}: {before}");
        }
        // 15 days after 2024-12-31 is 2025-01-15: falloff = min(366, 365).
        let jan15 = multiplier(Some(end + 1 + 14 * DAY), Some(Y2024), 1.0);
        assert!(approx(jan15, 1.0 + (366.0 - 15.0) / 366.0), "{jan15}");
    }

    #[test]
    fn beyond_the_falloff_is_neutral() {
        let (_, end) = Y2024;
        assert_eq!(multiplier(Some(end + 1 + 365 * DAY), Some(Y2024), 1.0), 1.0);
        assert_eq!(multiplier(Some(0), Some(Y2024), 1.0), 1.0);
        assert_eq!(multiplier(Some(i64::MAX), Some(Y2024), 1.0), 1.0);
        assert_eq!(multiplier(Some(i64::MIN), Some(Y2024), 1.0), 1.0);
    }

    #[test]
    fn open_ended_range_falloff_is_capped() {
        let (_, end) = Y2024;
        let open = (0, end);
        // ~20,000-day range: the fall-off still ends after MAX_FALLOFF_DAYS.
        let near = multiplier(Some(end + 1 + 14 * DAY), Some(open), 1.0);
        assert!(approx(near, 1.0 + (366.0 - 15.0) / 366.0), "{near}");
        assert_eq!(multiplier(Some(end + 1 + 400 * DAY), Some(open), 1.0), 1.0);
        assert_eq!(multiplier(Some(1_000 * DAY), Some(open), 1.0), 2.0);
    }

    #[test]
    fn decay_is_monotonic_with_distance() {
        let (_, end) = Y2024;
        let mut previous = multiplier(Some(end), Some(Y2024), 1.0);
        for days in 1..=400 {
            let m = multiplier(Some(end + days * DAY), Some(Y2024), 1.0);
            assert!(m <= previous, "day {days}: {m} > {previous}");
            assert!((1.0..=2.0).contains(&m), "day {days}: {m}");
            previous = m;
        }
    }
}
