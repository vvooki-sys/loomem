"""Evidence contract v2: pack returned records that carry engine metadata.

Experimental, separate from the frozen v1 `qa_context` module, whose behaviour,
hashes and signatures stay untouched. A v2 record is a dict with EXACTLY the
keys in EVIDENCE_FIELDS_V2; every field is present and unknown values are JSON
null, never a substituted date or role. Budget semantics, tokenizer identity,
serialization ("compact_json_then_html_escape_quote_false"), greedy-in-order
packing and dropped-record accounting are identical to v1; only the record
shape differs. Null fields are serialized literally (`"event_date":null`) and
therefore cost reader tokens: the design accepts that metadata, including
unknown metadata, is paid for out of the evidence budget, so a v2 context of
all-null records is strictly larger than the same records packed by v1.
"""
import datetime
import hashlib
import re

from qa_context import count_text, render_evidence, serialize_evidence, tokenizer_identity

EVIDENCE_FIELDS_V2 = ("record_id", "content", "author", "conversation_date", "event_date",
                      "event_date_provenance", "source_id", "byte_start", "byte_end")
METADATA_FIELDS_V2 = EVIDENCE_FIELDS_V2[2:]
EVENT_DATE_PROVENANCE = ("explicit", "resolved", "unknown")
_ISO_DATE = re.compile(r"^\d{4}-\d{2}-\d{2}$")


def _optional_text(value, field):
    if value is None:
        return None
    if not isinstance(value, str) or not value:
        raise ValueError(f"{field} must be a nonempty string or null")
    return value


def _optional_date(value, field):
    value = _optional_text(value, field)
    if value is None:
        return None
    if not _ISO_DATE.match(value):
        raise ValueError(f"{field} must be an ISO date YYYY-MM-DD as stored, or null")
    try:
        datetime.date.fromisoformat(value)
    except ValueError as error:
        raise ValueError(f"{field} is not a calendar date") from error
    return value


def _optional_offset(value, field):
    if value is None:
        return None
    if type(value) is not int or value < 0:
        raise ValueError(f"{field} must be a non-negative integer or null")
    return value


def validate_record_v2(record):
    """Return a canonical copy of one v2 record; raise ValueError on anything else."""
    if not isinstance(record, dict) or set(record) != set(EVIDENCE_FIELDS_V2):
        raise ValueError("Each v2 record must contain exactly the evidence v2 fields")
    identity, content = record["record_id"], record["content"]
    if not isinstance(identity, str) or not identity:
        raise ValueError("Record IDs must be nonempty strings")
    if not isinstance(content, str):
        raise ValueError("Evidence content must be the returned string")
    event_date = _optional_date(record["event_date"], "event_date")
    provenance = _optional_text(record["event_date_provenance"], "event_date_provenance")
    if (event_date is None) != (provenance is None):
        raise ValueError("event_date_provenance is set exactly when event_date is set")
    if provenance is not None and provenance not in EVENT_DATE_PROVENANCE:
        raise ValueError("event_date_provenance must be explicit, resolved or unknown")
    start = _optional_offset(record["byte_start"], "byte_start")
    end = _optional_offset(record["byte_end"], "byte_end")
    if (start is None) != (end is None) or (start is not None and end < start):
        raise ValueError("byte_start/byte_end must both be null or form a valid range")
    return {"record_id": identity, "content": content,
            "author": _optional_text(record["author"], "author"),
            "conversation_date": _optional_date(record["conversation_date"], "conversation_date"),
            "event_date": event_date, "event_date_provenance": provenance,
            "source_id": _optional_text(record["source_id"], "source_id"),
            "byte_start": start, "byte_end": end}


def validate_records_v2(records):
    """Validate a list of v2 records with unique IDs; never coerce values."""
    if not isinstance(records, list):
        raise ValueError("Evidence records must be a list")
    seen, validated = set(), []
    for record in records:
        checked = validate_record_v2(record)
        if checked["record_id"] in seen:
            raise ValueError("Record IDs must be unique")
        seen.add(checked["record_id"])
        validated.append(checked)
    return validated


def serialize_evidence_v2(records):
    """Same compact JSON as v1; null metadata is written literally."""
    return serialize_evidence(records)


def render_evidence_v2(context):
    """Same reader wire escaping as v1 (html.escape, quote=False)."""
    return render_evidence(context)


def pack_context_v2(records, token_budget, reader_model):
    """Rank-stable first-fit packing of complete v2 records, without truncation.

    Mirrors `qa_context.pack_context` step for step: each candidate is counted
    as the FULL serialized, HTML-escaped array (BPE counts are not additive),
    an oversized record is skipped and later smaller records may still fit,
    and dropped_record_tokens measures each dropped record alone in a
    one-element array. The only differences are the record shape and the
    extra `evidence_schema: 2` metadata key.
    """
    if type(token_budget) is not int or token_budget <= 0:
        raise ValueError("Context token budget must be a positive integer")
    records = validate_records_v2(records)
    context = serialize_evidence_v2([])
    context_tokens = count_text(render_evidence_v2(context), reader_model)
    if context_tokens > token_budget:
        raise ValueError("Budget cannot fit the empty serialized evidence array")
    selected, dropped, dropped_tokens = [], [], {}
    for record in records:
        candidate = serialize_evidence_v2(selected + [record])
        tokens = count_text(render_evidence_v2(candidate), reader_model)
        if tokens <= token_budget:
            selected.append(record)
            context, context_tokens = candidate, tokens
        else:
            dropped.append(record["record_id"])
            dropped_tokens[record["record_id"]] = count_text(
                render_evidence_v2(serialize_evidence_v2([record])), reader_model)
    return {"schema_version": 1, "packing_policy": "ranked_full_records_first_fit_v1",
            "evidence_schema": 2,
            "context": context, "context_tokens": context_tokens,
            "context_sha256": hashlib.sha256(context.encode("utf-8")).hexdigest(),
            "rendered_context_sha256": hashlib.sha256(render_evidence_v2(context).encode("utf-8")).hexdigest(),
            "serialization": "compact_json_then_html_escape_quote_false",
            "context_token_budget": token_budget, "budget_scope": "rendered_evidence_only",
            "selected_ids": [record["record_id"] for record in selected],
            "dropped_ids": dropped, "dropped_record_tokens": dropped_tokens,
            "empty": not selected, "tokenizer": tokenizer_identity(reader_model)}
