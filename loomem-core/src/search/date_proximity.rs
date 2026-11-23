//! Event-date proximity boost (`[search] event_date_proximity_boost`).
//!
//! A query with a date range (`date_from`/`date_to`, or a date parsed out of
//! the query text) admits chunks by date but, until this lever, ranked them
//! as if the range did not exist. [`multiplier`] turns the distance between a
//! chunk's event time and the range into a score multiplier:
//!
//! - inside the range (edges included): `1.0 + weight`;
//! - outside: linear decay from `1.0 + weight` at the nearer edge down to
//!   `1.0` at a distance of one span, where
//!   `span = max(end - start, 86_400)` seconds (one day minimum, so a
//!   single-day range still has a usable fall-off);
//! - no event time, no range, or a weight that is not a positive finite
//!   number: exactly `1.0`.
//!
//! With the shipped default `weight = 0.0` the multiplier is exactly `1.0` on
//! every branch, and `score * 1.0 == score` in IEEE 754, so ranking is
//! unchanged. The function is pure; the caller decides where the event time
//! and the range come from.

/// Minimum span, in seconds, used for the outside-range fall-off.
const MIN_SPAN_SECS: i64 = 86_400;

/// Whether `weight` turns the boost on: a positive finite number. Callers
/// can use it to skip loading the event time when the lever is off.
#[must_use]
pub fn enabled(weight: f64) -> bool {
    weight.is_finite() && weight > 0.0
}

/// Score multiplier for a chunk whose event happened at `event_ts` (unix
/// seconds), given the query range `range = (start, end)` (unix seconds,
/// inclusive) and the configured `weight`.
///
/// Returns exactly `1.0` when `weight` is not a positive finite number, when
/// `event_ts` or `range` is `None`, or when the event lies one span or more
/// outside the range. A reversed range (`start > end`) is treated as its
/// normalised form.
#[must_use]
pub fn multiplier(event_ts: Option<i64>, range: Option<(i64, i64)>, weight: f64) -> f64 {
    if !enabled(weight) {
        return 1.0;
    }
    let (Some(event), Some((a, b))) = (event_ts, range) else {
        return 1.0;
    };
    let (start, end) = if a <= b { (a, b) } else { (b, a) };
    let distance = if event < start {
        start.saturating_sub(event)
    } else if event > end {
        event.saturating_sub(end)
    } else {
        return 1.0 + weight;
    };
    let span = end.saturating_sub(start).max(MIN_SPAN_SECS);
    if distance >= span {
        return 1.0;
    }
    1.0 + weight * remaining_fraction(span - distance, span)
}

/// `remaining / span` for `0 < remaining <= span`, computed in exact integer
/// arithmetic at parts-per-billion resolution so no lossy numeric cast is
/// needed (unix-second spans can exceed what `f64::from` accepts).
fn remaining_fraction(remaining: i64, span: i64) -> f64 {
    const PPB: i128 = 1_000_000_000;
    let ppb = i128::from(remaining) * PPB / i128::from(span);
    u32::try_from(ppb).map_or(0.0, |ppb| f64::from(ppb) / 1e9)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: i64 = 86_400;
    /// 2024-01-01T00:00:00Z ..= 2024-12-31T23:59:59Z (366-day leap year).
    const Y2024: (i64, i64) = (1_704_067_200, 1_735_689_599);

    fn approx(actual: f64, expected: f64) -> bool {
        (actual - expected).abs() < 1e-6
    }

    /// Every branch of [`multiplier`] for one weight.
    fn cases() -> Vec<(&'static str, Option<i64>, Option<(i64, i64)>)> {
        let (start, end) = Y2024;
        let span = end - start;
        vec![
            ("no event", None, Some(Y2024)),
            ("no range", Some(start), None),
            ("neither", None, None),
            ("inside", Some(start + 150 * DAY), Some(Y2024)),
            ("start edge", Some(start), Some(Y2024)),
            ("end edge", Some(end), Some(Y2024)),
            ("half span before", Some(start - span / 2), Some(Y2024)),
            ("half span after", Some(end + span / 2), Some(Y2024)),
            ("exactly one span after", Some(end + span), Some(Y2024)),
            ("far before", Some(0), Some(Y2024)),
            ("far after", Some(i64::MAX), Some(Y2024)),
            ("far before i64::MIN", Some(i64::MIN), Some(Y2024)),
            (
                "reversed range inside",
                Some(start + DAY),
                Some((end, start)),
            ),
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
        assert_eq!(multiplier(Some(end), Some(Y2024), 0.5), 1.5);
        assert_eq!(multiplier(Some(start + DAY), Some((end, start)), 1.0), 2.0);
    }

    #[test]
    fn half_a_span_outside_gets_half_the_boost() {
        let (start, end) = Y2024;
        let span = end - start;
        assert!(approx(
            multiplier(Some(end + span / 2), Some(Y2024), 1.0),
            1.5
        ));
        assert!(approx(
            multiplier(Some(start - span / 2), Some(Y2024), 1.0),
            1.5
        ));
        assert!(approx(
            multiplier(Some(end + span / 4), Some(Y2024), 2.0),
            2.5
        ));
    }

    #[test]
    fn one_span_or_farther_outside_is_neutral() {
        let (start, end) = Y2024;
        let span = end - start;
        assert_eq!(multiplier(Some(end + span), Some(Y2024), 1.0), 1.0);
        assert_eq!(multiplier(Some(end + 10 * span), Some(Y2024), 1.0), 1.0);
        assert_eq!(multiplier(Some(0), Some(Y2024), 1.0), 1.0);
        assert_eq!(multiplier(Some(i64::MAX), Some(Y2024), 1.0), 1.0);
        assert_eq!(multiplier(Some(i64::MIN), Some(Y2024), 1.0), 1.0);
    }

    #[test]
    fn single_day_range_falls_off_over_one_day() {
        // 2024-06-01 as a zero-width range (date_from == date_to at midnight).
        let day = 1_717_200_000;
        assert_eq!(multiplier(Some(day), Some((day, day)), 1.0), 2.0);
        assert!(approx(
            multiplier(Some(day + DAY / 2), Some((day, day)), 1.0),
            1.5
        ));
        assert_eq!(multiplier(Some(day + DAY), Some((day, day)), 1.0), 1.0);
        assert_eq!(multiplier(Some(day - 2 * DAY), Some((day, day)), 1.0), 1.0);
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
