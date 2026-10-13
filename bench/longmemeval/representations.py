"""Offline, label-blind source/fact representations. No extraction or network I/O."""
import hashlib
import json


def _json(value):
    return json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":"))


def _text(value, name):
    if not isinstance(value, str) or not value:
        raise ValueError(f"{name} must be a nonempty string")
    return value


def _optional_text(value, name):
    return None if value is None else _text(value, name)


def _sources(value):
    if not isinstance(value, list):
        raise ValueError("sources must be a list; use [] for unknown provenance")
    result = []
    for source in value:
        session = _text(source["session_id"], "session_id")
        turn = source["turn_index"]
        if type(turn) is not int or turn < 0:
            raise ValueError("turn_index must be a nonnegative integer")
        item = {"session_id": session, "turn_index": turn}
        if item not in result:
            result.append(item)
    return sorted(result, key=lambda item: (item["session_id"], item["turn_index"]))


def _record(question_id, kind, content, sources, metadata):
    record = {"kind": kind, "content": _text(content, "content"),
              "sources": _sources(sources),
              "role": _optional_text(metadata.get("role"), "role"),
              "date": _optional_text(metadata.get("date"), "date")}
    record["record_id"] = hashlib.sha256(_json([question_id, record]).encode()).hexdigest()
    return record


def _raw_records(question):
    question_id = _text(question["question_id"], "question_id")
    sessions = question["haystack_sessions"]
    ids = question["haystack_session_ids"]
    dates = question.get("haystack_dates", [None] * len(sessions))
    if not isinstance(sessions, list) or not isinstance(ids, list) or not isinstance(dates, list):
        raise ValueError("sessions, session IDs and dates must be lists")
    if not len(sessions) == len(ids) == len(dates) or len(set(ids)) != len(ids):
        raise ValueError("session IDs/dates must align one-to-one with sessions")
    records = []
    for session, session_id, date in zip(sessions, ids, dates):
        _text(session_id, "session_id")
        if not isinstance(session, list):
            raise ValueError("each session must be a list of turns")
        for index, turn in enumerate(session):
            metadata = {"role": turn.get("role"), "date": date}
            records.append(_record(question_id, "source", turn["content"],
                                   [{"session_id": session_id, "turn_index": index}], metadata))
    return records


def build_records(question, mode, facts=None):
    """Build raw/facts/dual records; never copy answer or has_answer labels.

    Facts are externally supplied, not inferred. Known references must identify
    actual turns. Empty fact sources explicitly represent legacy/unknown origin.
    Dates are session dates, never asserted to be dates of the described event.
    """
    if mode not in {"raw", "facts", "dual"}:
        raise ValueError("mode must be raw, facts or dual")
    raw = _raw_records(question)
    if mode == "raw":
        return raw
    if facts is None or not isinstance(facts, list):
        raise ValueError("facts and dual require an explicit external facts list")
    known = {(s["session_id"], s["turn_index"]) for r in raw for s in r["sources"]}
    derived = []
    for fact in facts:
        record = _record(question["question_id"], "fact", fact["content"],
                         fact.get("sources", []), fact)
        if any((s["session_id"], s["turn_index"]) not in known for s in record["sources"]):
            raise ValueError("fact references an unknown source turn")
        derived.append(record)
    return (raw if mode == "dual" else []) + derived


def serialize_records(records):
    """Serialize only permitted evidence fields; records remain untrusted data."""
    return _json([{key: record[key] for key in
                   ("record_id", "kind", "content", "sources", "role", "date")}
                  for record in records])


def pack_records(records, budget_tokens, count_tokens):
    """Greedy skip-large packing using the caller's actual tokenizer.

    Counts the COMPLETE evidence serialization (not reader prompt/question).
    Never truncates content or collapses records across different provenance.
    Caller must bind tokenizer identity/settings in its run manifest.
    """
    if type(budget_tokens) is not int or budget_tokens < 0:
        raise ValueError("budget_tokens must be a nonnegative integer")
    def count(context):
        size = count_tokens(context)
        if type(size) is not int or size < 0:
            raise ValueError("token counter must return a nonnegative integer")
        return size
    selected, dropped, seen = [], [], set()
    context = serialize_records([])
    size = count(context)
    if size > budget_tokens:
        raise ValueError("budget cannot fit the empty evidence envelope")
    for record in records:
        identity = _json({k: record[k] for k in ("kind", "content", "sources", "role", "date")})
        if identity in seen:
            dropped.append(record["record_id"])
            continue
        seen.add(identity)
        candidate = serialize_records(selected + [record])
        candidate_size = count(candidate)
        if candidate_size > budget_tokens:
            dropped.append(record["record_id"])
            continue
        selected.append(record)
        context, size = candidate, candidate_size
    return {"records": selected, "context": context, "token_count": size,
            "dropped_record_ids": dropped}
