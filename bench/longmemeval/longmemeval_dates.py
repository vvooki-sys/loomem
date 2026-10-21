"""Dataset date normalisation shared by both experiment arms.

LongMemEval ships session dates as `YYYY/MM/DD (Www) HH:MM` (the
`haystack_dates` form, e.g. `2023/05/20 (Sat) 02:21`). Evidence contract v2
and the engine's `memory_ingest` both take ISO calendar dates. `to_iso` is the
single adapter-boundary conversion used by the E0 control arm (when it parses
the stored turn envelope back) and by the E1 ingest arm (when it passes
`conversation_date`), so the two arms see the same date for the same session.

It converts exactly that one dataset pattern, passes a value that already is
`YYYY-MM-DD` through unchanged, and returns None for anything else: a
near-miss such as `2023/5/20`, a weekday that does not match the date, an
impossible calendar date or a non-string are never guessed at. Engine-returned
dates are never passed through this function; `validate_record_v2` keeps
rejecting a non-ISO engine date, so normalisation is confined to dataset input.
"""
import datetime
import re

DATASET_DATE = re.compile(r"^(\d{4})/(\d{2})/(\d{2}) \((Mon|Tue|Wed|Thu|Fri|Sat|Sun)\) (\d{2}):(\d{2})$")
ISO_DATE = re.compile(r"^\d{4}-\d{2}-\d{2}$")
WEEKDAYS = ("Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun")


def _calendar_date(year, month, day):
    try:
        return datetime.date(year, month, day)
    except ValueError:
        return None


def to_iso(value):
    """Return `YYYY-MM-DD` for a dataset or ISO date string, else None."""
    if not isinstance(value, str):
        return None
    if ISO_DATE.match(value):
        parsed = _calendar_date(*(int(part) for part in value.split("-")))
        return value if parsed is not None and parsed.isoformat() == value else None
    match = DATASET_DATE.match(value)
    if match is None:
        return None
    parsed = _calendar_date(*(int(match.group(i)) for i in (1, 2, 3)))
    if parsed is None or WEEKDAYS[parsed.weekday()] != match.group(4):
        return None
    hour, minute = int(match.group(5)), int(match.group(6))
    if hour > 23 or minute > 59:
        return None
    return parsed.isoformat()
