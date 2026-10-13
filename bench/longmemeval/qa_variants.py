"""Label-blind retrieval variants over ACTUAL engine-returned sanitized text.

This module never reads dataset turns, labels or gold answers. Callers must pass
/v1/search text verbatim; optional neighbor text must come from the same live
engine via /get, with provenance joined by stable record ID. No corpus fallback.
"""

VARIANTS = {"baseline20": 20, "expanded60": 60, "expanded60_neighbors": 60}


def candidate_limit(variant):
    """Number to request from the engine; obtain each arm's ranking independently."""
    if variant not in VARIANTS:
        raise ValueError("Unknown retrieval variant")
    return VARIANTS[variant]


def _record(value):
    result = {key: value[key] for key in ("record_id", "content")}
    if any(not isinstance(item, str) or not item for item in result.values()):
        raise ValueError("Evidence ID/content must be nonempty strings")
    return result


def _neighbors(records, engine_sources):
    if engine_sources is None:
        raise ValueError("Neighbor variant requires actual engine-fetched source text")
    by_id, by_turn = {}, {}
    for item in engine_sources:
        record = _record(item)
        session, turn = item["session_id"], item["turn_index"]
        if not isinstance(session, str) or not session or type(turn) is not int or turn < 0:
            raise ValueError("Source provenance must identify a session and nonnegative turn")
        if record["record_id"] in by_id or (session, turn) in by_turn:
            raise ValueError("Ambiguous source provenance")
        by_id[record["record_id"]] = (session, turn, record)
        by_turn[(session, turn)] = record
    expanded = []
    for record in records:
        expanded.append(record)
        source = by_id.get(record["record_id"])
        if source is None:
            raise ValueError("Missing source provenance for a retrieved record")
        session, turn, stored = source
        if stored != record:
            raise ValueError("Search/get content mismatch; cannot expand changed evidence")
        for neighbor in (turn - 1, turn + 1):
            if (session, neighbor) in by_turn:
                expanded.append(by_turn[(session, neighbor)])
    return expanded


def _unique(records):
    seen, selected = {}, []
    for record in records:
        previous = seen.get(record["record_id"])
        if previous is not None:
            if previous != record:
                raise ValueError("Conflicting text for the same evidence ID")
            continue
        seen[record["record_id"]] = record
        selected.append(record)
    return selected


def neighbor_request_ids(retrieved_ids, provenance_records):
    """IDs of ±1 turns to fetch, using label-blind ingest provenance only.

    provenance_records is the raw build_records list. Text is never inspected.
    Returned IDs exclude seeds and duplicates, in deterministic seed/prev/next
    order. Missing/ambiguous raw provenance fails rather than inventing linkage.
    """
    by_id, by_turn = {}, {}
    for item in provenance_records:
        sources = item.get("sources", [])
        if item.get("kind") != "source" or len(sources) != 1:
            raise ValueError("Neighbor expansion requires single-turn raw provenance")
        source = sources[0]
        location = (source["session_id"], source["turn_index"])
        if (not isinstance(location[0], str) or not location[0]
                or type(location[1]) is not int or location[1] < 0):
            raise ValueError("Invalid source provenance")
        if item["record_id"] in by_id or location in by_turn:
            raise ValueError("Ambiguous source provenance")
        by_id[item["record_id"]] = location
        by_turn[location] = item["record_id"]
    requested, seen = [], set(retrieved_ids)
    for identity in retrieved_ids:
        if identity not in by_id:
            raise ValueError("Retrieved ID has no source provenance")
        session, turn = by_id[identity]
        for adjacent in (turn - 1, turn + 1):
            neighbor = by_turn.get((session, adjacent))
            if neighbor is not None and neighbor not in seen:
                requested.append(neighbor)
                seen.add(neighbor)
    return requested


def variant_records(variant, retrieved_records, engine_sources=None):
    """Return ordered minimal evidence for the canonical qa_context packer.

    Input search and optional GET content must be actual engine-returned text.
    No truncation or token accounting occurs here. Neighbors follow each seed in
    previous/next order, radius one, same session. This is a single-question API.
    """
    limit = candidate_limit(variant)
    ranked = [_record(record) for record in retrieved_records]
    if len(ranked) > limit:
        raise ValueError("Engine returned more than the arm's requested candidate limit")
    candidates = _neighbors(ranked, engine_sources) if variant.endswith("_neighbors") else ranked
    return _unique(candidates)
