"""Map engine responses to evidence contract v2 records.

`from_engine_hit` consumes one `/v1/search` result (`results[]`). The engine
may attach an additive `evidence` object carrying the v2 metadata fields
(same names as the record minus `record_id`; the engine `id` is mapped to the
benchmark record ID by the caller). A hit without that object is still
representable: every metadata field is null. Nothing is coerced: an unexpected
type, key or value raises ValueError so a run fails at the retrieval stage
instead of silently rendering wrong metadata.

`from_e0_turn_json` is a CONTROL-ARM convenience only. The E0 baseline stored
each turn as `json.dumps({"role", "date", "content"})` (see
`runner_local.ingest`), so the returned text is that envelope. Parsing it back
lets the E0 arm be rendered through the same v2 renderer with the same fields
(role -> author, date -> conversation_date, event fields null). It is applied
only when the returned string is exactly such an object, byte for byte what
that ingest wrote; any other string stays verbatim with null metadata. The
envelope date is DATASET input (the `haystack_dates` string the ingest copied
verbatim), so it is normalised at this adapter boundary with
`longmemeval_dates.to_iso`, the same function the E1 ingest arm uses for
`conversation_date`; a date that is neither the dataset pattern nor ISO
becomes null, never a guess. Engine-returned dates (`from_engine_hit`,
`from_memory_item`) are never reformatted: `validate_record_v2` rejects them
unless they already are ISO calendar dates.
"""
import json

from longmemeval_dates import to_iso
from qa_context_v2 import METADATA_FIELDS_V2, validate_record_v2

E0_ENVELOPE_KEYS = frozenset({"role", "date", "content"})


def null_metadata():
    return {field: None for field in METADATA_FIELDS_V2}


def _record(record_id, content, metadata):
    return validate_record_v2({"record_id": record_id, "content": content, **metadata})


def _evidence_object(container, source):
    """Return the validated `evidence` sub-object, or null metadata when absent."""
    if not isinstance(container, dict):
        raise ValueError(f"{source} must be a JSON object")
    evidence = container.get("evidence")
    if evidence is None:
        return null_metadata()
    if not isinstance(evidence, dict) or set(evidence) != set(METADATA_FIELDS_V2):
        raise ValueError(f"{source} evidence must carry exactly the v2 metadata fields")
    return dict(evidence)


def from_engine_hit(hit, record_id):
    """Map one `/v1/search` hit (with or without `evidence`) to a v2 record."""
    metadata = _evidence_object(hit, "Search hit")
    if not isinstance(hit.get("id"), str) or not hit["id"]:
        raise ValueError("Search hit must carry a string engine id")
    if not isinstance(hit.get("content"), str):
        raise ValueError("Search hit content must be the returned string")
    return _record(record_id, hit["content"], metadata)


def from_memory_item(item, record_id):
    """Map one `GET /api/memories/<id>` item to a v2 record.

    With `[search] evidence_fields = true` the endpoint attaches the same
    additive `evidence` object as `/v1/search` hits and it is mapped
    identically. Without the flag (the engine default) the object is absent
    and every metadata field is null; the item's top-level `event_date` has
    no provenance and is therefore NOT lifted (the benchmark must not assert
    a provenance the engine did not state).
    """
    metadata = _evidence_object(item, "Memory item")
    if not isinstance(item.get("id"), str) or not item["id"]:
        raise ValueError("Memory item must carry a string engine id")
    if not isinstance(item.get("content"), str):
        raise ValueError("Memory item content must be the stored string")
    return _record(record_id, item["content"], metadata)


def _e0_envelope(content):
    try:
        parsed = json.loads(content)
    except ValueError:
        return None
    if (not isinstance(parsed, dict) or set(parsed) != E0_ENVELOPE_KEYS
            or not all(isinstance(parsed[key], str) for key in E0_ENVELOPE_KEYS)):
        return None
    # Exactly the bytes `runner_local.ingest` wrote (default json.dumps, this key
    # order); anything looser, e.g. trailing whitespace, is not an E0 envelope.
    if json.dumps({key: parsed[key] for key in ("role", "date", "content")}) != content:
        return None
    return parsed


def from_e0_turn_json(content, record_id):
    """Control-arm mapping of an E0 `{"role","date","content"}` envelope.

    A non-envelope string is returned verbatim with null metadata. An envelope
    yields author=role, conversation_date=to_iso(date) and the inner text as
    content; event_date, its provenance, source_id and byte offsets stay null
    because the E0 ingest never recorded them.
    """
    if not isinstance(content, str):
        raise ValueError("E0 content must be the returned string")
    envelope = _e0_envelope(content)
    if envelope is None:
        return _record(record_id, content, null_metadata())
    metadata = null_metadata()
    metadata.update(author=envelope["role"], conversation_date=to_iso(envelope["date"]))
    return _record(record_id, envelope["content"], metadata)
