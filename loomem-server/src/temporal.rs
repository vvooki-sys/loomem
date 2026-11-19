use jiff::{civil::Date, civil::Time, tz::TimeZone, Timestamp, ZonedDateTime};

pub(crate) use crate::handlers::date_filter::{now_unix_seconds, unix_seconds_to_i64};

pub(crate) fn utc_date(timestamp: Timestamp) -> Date {
    timestamp.to_zoned_datetime(TimeZone::UTC).date()
}

pub(crate) fn utc_timestamp(date: Date, hour: i8) -> Option<i64> {
    let hour = i64::from(Time::new(hour, 0, 0, 0).ok()?.hour());
    let epoch = Date::new(1970, 1, 1).ok()?;
    let days = i64::from(date.since(epoch).ok()?.get_days());
    days.checked_mul(86_400)?
        .checked_add(hour.checked_mul(3_600)?)
}

/// Match chrono's `to_rfc3339()` for UTC: AutoSi precision and `+00:00`.
pub(crate) fn to_rfc3339(timestamp: Timestamp) -> String {
    let zoned = timestamp.to_zoned_datetime(TimeZone::UTC);
    format!(
        "{}{}+00:00",
        zoned.strftime("%Y-%m-%dT%H:%M:%S"),
        auto_si_fraction(&zoned)
    )
}

/// Match chrono's serde timestamp format: AutoSi precision and `Z`.
pub(crate) fn to_rfc3339_z(timestamp: Timestamp) -> String {
    let zoned = timestamp.to_zoned_datetime(TimeZone::UTC);
    format!(
        "{}{}Z",
        zoned.strftime("%Y-%m-%dT%H:%M:%S"),
        auto_si_fraction(&zoned)
    )
}

fn auto_si_fraction(zoned: &ZonedDateTime) -> String {
    let nanos = zoned.subsec_nanosecond();
    if nanos == 0 {
        String::new()
    } else if nanos % 1_000_000 == 0 {
        format!(".{:03}", nanos / 1_000_000)
    } else if nanos % 1_000 == 0 {
        format!(".{:06}", nanos / 1_000)
    } else {
        format!(".{nanos:09}")
    }
}
