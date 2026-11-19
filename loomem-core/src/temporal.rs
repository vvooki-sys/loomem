use jiff::{
    civil::{Date, Time},
    tz::{Offset, TimeZone},
    Timestamp, ZonedDateTime,
};

pub(crate) use crate::date_resolver::{now_unix_seconds, unix_seconds_to_u64};

pub(crate) fn utc_date(timestamp: Timestamp) -> Date {
    timestamp.to_zoned_datetime(TimeZone::UTC).date()
}

pub(crate) fn utc_timestamp(date: Date, hour: i8) -> Option<i64> {
    let time = Time::new(hour, 0, 0, 0).ok()?;
    let zoned = date
        .to_datetime(time)
        .to_zoned_datetime(TimeZone::UTC)
        .ok()?;
    Some(zoned.timestamp().as_second())
}

/// Match chrono's `to_rfc3339()` for UTC: AutoSi precision and `+00:00`.
pub(crate) fn to_rfc3339(timestamp: Timestamp) -> String {
    to_rfc3339_with_offset(timestamp, Offset::UTC)
}

/// Match chrono's `to_rfc3339()` while preserving the input numeric offset.
pub(crate) fn to_rfc3339_with_offset(timestamp: Timestamp, offset: Offset) -> String {
    let zoned = timestamp.to_zoned_datetime(TimeZone::fixed(offset));
    format!(
        "{}{}{}",
        zoned.strftime("%Y-%m-%dT%H:%M:%S"),
        auto_si_fraction(&zoned),
        zoned.strftime("%:z")
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
