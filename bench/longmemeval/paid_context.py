"""Gold-blind retrieval context of one question: facts, originals menu, evidence packing.

Everything here is derived from one ``/v1/search`` response (``top_k=60``,
``dry_run=true``, ``trace=true``, ``reference_time`` from the question date)
and the stored source contents of the question's history. Facts are the hits
with ``metadata.level == 1``; the originals menu is the ENGINE-ranked list of
fragment hits (``<uuid>:f<k>``, level 0) whose content must equal the exact
UTF-8 byte slice ``sources[source_id].encode()[byte_start:byte_end]`` -- any
other byte is a ``ContextError`` (the question becomes a technical error for
all arms and stays in the denominator). No gold field ever enters this module:
the only question fields used are the execution view's.
"""
import hashlib
import json
import re

from evidence_v2 import from_engine_hit
from qa_context_v2 import validate_records_v2

FRAGMENT_ID = re.compile(r"^[0-9a-f-]{36}:f(?:0|[1-9][0-9]*)$")
MAX_SELECTED = 16
EXECUTION_KEYS = ("question_id", "history_id", "question", "question_date")
# User-payload key -> sheet cap that bounds its serialised size (plan §9 caps).
CAP_OF_KEY = {"question": "QUESTION_CAP", "reference": "QUESTION_CAP",
              "facts": "FACTS_JSON_CAP", "diagnostic": "DIAG_CAP", "first_diagnostic": "DIAG_CAP",
              "proposed_answer": "ANSWER_CAP", "candidate": "ANSWER_CAP",
              "original_records": "ORIGINALS_MENU_CAP", "evidence": "EVIDENCE_CAP"}


class ContextError(ValueError):
    """Technical error of the question (all arms); the question stays in the denominator."""


def compact_json(value):
    return json.dumps(value, ensure_ascii=False, separators=(",", ":"), allow_nan=False)


def byte_size(value):
    text = value if isinstance(value, str) else compact_json(value)
    return len(text.encode("utf-8"))


def _hits(search_json):
    if not isinstance(search_json, dict) or not isinstance(search_json.get("results"), list):
        raise ContextError("search response lacks a results list")
    hits = search_json["results"]
    for hit in hits:
        if not isinstance(hit, dict) or not isinstance(hit.get("id"), str) or not hit["id"]:
            raise ContextError("search hit without a string id")
        if not isinstance(hit.get("metadata"), dict) or type(hit["metadata"].get("level")) is not int:
            raise ContextError(f"search hit {hit.get('id')!r} lacks an integer metadata.level")
    return hits


def _record(hit):
    try:
        return from_engine_hit(hit, hit["id"])
    except ValueError as exc:
        raise ContextError(f"search hit {hit['id']!r}: {exc}") from exc


def facts_records(search_json, source_ids):
    """Level-1 hits as v2 records in engine rank order; byte offsets are null (facts are derived)."""
    records = []
    for hit in _hits(search_json):
        if hit["metadata"]["level"] != 1:
            continue
        record = _record(hit)
        if record["source_id"] is not None and record["source_id"] not in source_ids:
            raise ContextError(f"fact {hit['id']!r} cites source {record['source_id']!r} outside the history")
        record["byte_start"] = record["byte_end"] = None
        records.append(record)
    try:
        return validate_records_v2(records)
    except ValueError as exc:
        raise ContextError(f"facts: {exc}") from exc


def _exact_slice(record, sources):
    sid, start, end = record["source_id"], record["byte_start"], record["byte_end"]
    if sid is None or start is None:
        raise ContextError(f"fragment {record['record_id']!r} lacks source_id or byte offsets")
    if sid not in sources or not record["record_id"].startswith(sid + ":f"):
        raise ContextError(f"fragment {record['record_id']!r} does not belong to a stored source of this history")
    expected = sources[sid].encode("utf-8")[start:end]
    if record["content"].encode("utf-8") != expected or end > len(sources[sid].encode("utf-8")):
        raise ContextError(f"fragment {record['record_id']!r} content differs from the stored byte slice")


def originals_menu(search_json, sources, cap_bytes):
    """Fragment hits (level 0, ``<uuid>:f<k>``) in engine rank order, each an exact byte slice;
    the serialised menu stops before it would exceed ``cap_bytes``."""
    if type(cap_bytes) is not int or cap_bytes <= 0:
        raise ContextError("menu cap must be a positive int")
    menu = []
    for hit in _hits(search_json):
        is_fragment = bool(FRAGMENT_ID.match(hit["id"]))
        if is_fragment and hit["metadata"]["level"] != 0:
            raise ContextError(f"fragment {hit['id']!r} reported with level {hit['metadata']['level']}")
        if not is_fragment:
            continue
        record = _record(hit)
        _exact_slice(record, sources)
        if byte_size(menu + [record]) > cap_bytes:
            break
        menu.append(record)
    try:
        return validate_records_v2(menu)
    except ValueError as exc:
        raise ContextError(f"originals menu: {exc}") from exc


def pack_evidence(facts, originals_menu, selected_ids, cap_bytes, max_selected=MAX_SELECTED):
    """Facts are never evicted (facts alone above the cap -> ContextError). Selected ids are
    deduplicated, unknown ones go to ``rejected_ids``, at most ``max_selected`` (None = all)
    are kept in menu rank order and each is added while the evidence still fits."""
    if type(cap_bytes) is not int or cap_bytes <= 0:
        raise ContextError("evidence cap must be a positive int")
    try:
        facts = validate_records_v2(facts)
        menu = validate_records_v2(originals_menu)
    except ValueError as exc:
        raise ContextError(f"evidence: {exc}") from exc
    if not isinstance(selected_ids, list) or any(not isinstance(i, str) for i in selected_ids):
        raise ContextError("selected_ids must be a list of strings")
    by_id = {r["record_id"]: r for r in menu}
    wanted = list(dict.fromkeys(selected_ids))
    rejected = [i for i in wanted if i not in by_id]
    ranked = [i for i in by_id if i in wanted]
    if max_selected is not None:
        rejected.extend(ranked[max_selected:])
        ranked = ranked[:max_selected]
    originals = []
    if byte_size({"facts": facts, "originals": originals}) > cap_bytes:
        raise ContextError(f"facts alone exceed the evidence cap of {cap_bytes} bytes")
    for identity in ranked:
        candidate = originals + [by_id[identity]]
        if byte_size({"facts": facts, "originals": candidate}) <= cap_bytes:
            originals = candidate
    return {"facts": facts, "originals": originals, "rejected_ids": rejected,
            "bytes": byte_size({"facts": facts, "originals": originals})}


def byte_caps_ok(stage, parts, caps):
    """Raise ContextError when any user-payload part exceeds its sheet cap."""
    if not isinstance(parts, dict):
        raise ContextError(f"stage {stage!r}: parts must be a dict")
    for key, value in parts.items():
        cap_name = CAP_OF_KEY.get(key)
        if cap_name is None:
            continue
        if cap_name not in caps:
            raise ContextError(f"stage {stage!r}: caps lack {cap_name}")
        size = byte_size(value)
        if size > caps[cap_name]:
            raise ContextError(f"stage {stage!r}: {key} is {size} bytes, above {cap_name} {caps[cap_name]}")


def context_sha256(context):
    body = {k: v for k, v in context.items() if k != "context_sha256"}
    return hashlib.sha256(json.dumps(body, sort_keys=True, ensure_ascii=False, separators=(",", ":"),
                                     allow_nan=False).encode("utf-8")).hexdigest()


def build_context(view, search_json, sources, caps, zero_facts):
    """The ``contexts/<qid>.json`` object: execution view + facts + menu + source ids + hash."""
    if not isinstance(view, dict) or set(view) != set(EXECUTION_KEYS):
        raise ContextError(f"view must carry exactly {EXECUTION_KEYS} (execution view, no gold)")
    if not isinstance(sources, dict) or any(not isinstance(v, str) for v in sources.values()):
        raise ContextError("sources must map source_id -> stored content")
    byte_caps_ok("retrieval", {"question": view["question"]}, caps)
    facts = facts_records(search_json, set(sources))
    byte_caps_ok("retrieval", {"facts": facts}, caps)
    menu = originals_menu(search_json, sources, caps["ORIGINALS_MENU_CAP"])
    context = {**{k: view[k] for k in EXECUTION_KEYS}, "facts": facts, "menu": menu,
               "source_ids": sorted(sources), "zero_facts": bool(zero_facts) or not facts}
    context["context_sha256"] = context_sha256(context)
    return context
