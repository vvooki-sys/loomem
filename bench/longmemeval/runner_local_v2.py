"""Owned local adapter extension that also captures evidence contract v2.

`runner_local.LocalEngineAdapter` is frozen: this subclass changes none of its
behaviour. It overrides only the `/search` branch, repeating the v1 wire call
and v1 result shape verbatim (the equality is pinned by tests). When
`capture_context` is on, the result additionally carries
`retrieved_records_v2`, the same hits mapped through
`evidence_v2.from_engine_hit`, next to the unchanged v1 `retrieved_records`
that `runner.retrieve` keeps validating. `fetch_records_v2` is the neighbor
counterpart over `GET /api/memories/<id>`; with `[search] evidence_fields =
true` that endpoint attaches the same `evidence` object as search hits and
the records carry real metadata, without the flag they carry null metadata.
Text is never reconstructed: every `content` is the string the engine returned.

Rejections: the engine's `trace_metadata.rejections` (`{chunk_id, stage,
reason}` with raw engine ids) are mapped through the adapter's id mapping and
returned at the TOP LEVEL of the search result as `rejections`, a sibling of
`retrieved_ids`/`trace_metadata`. The frozen v1 `runner.trace_projection`
keeps only `coverage` and `stages` of `evidence`, so a key inside the trace
would never reach the sidecar; `stage_diagnostics` reads a top-level
`rejections` on each row. `write_trace_row_v2` persists such a row: the v1
projection unchanged plus the mapped rejections. Entries whose id is unknown
to the mapping are dropped (the engine already scrubs out-of-scope ids; an
unmapped id can only be a chunk the adapter did not write for this question).
"""
import json
from pathlib import Path
from urllib.parse import quote

from evidence_v2 import from_engine_hit, from_memory_item
from qa_context_v2 import validate_records_v2
from runner import id_list, trace_projection
from runner_local import LocalEngineAdapter
from validate import canonical_hash, decode, fail

REJECTION_KEYS = frozenset({"chunk_id", "stage", "reason"})


def context_records_v2(value, ids):
    """Validate exact returned v2 evidence against the returned IDs and rank."""
    if not isinstance(value, list):
        fail("Missing actual returned v2 context records")
    try:
        records = validate_records_v2(value)
    except ValueError as error:
        fail(f"Context records violate evidence contract v2: {error}")
    if id_list([record["record_id"] for record in records]) != ids:
        fail("Actual v2 context does not preserve returned IDs and rank")
    return records


def context_artifact_v2(found, row, manifest_hash, ids):
    return {"schema_version": 2, "question_id": row["question_id"],
            "manifest_sha256": manifest_hash,
            "records": context_records_v2(found.get("retrieved_records_v2"), ids)}


def load_context_artifact_v2(path, trace, manifest_hash):
    """Verify an immutable v2 context against its manifest-bound ID-only trace."""
    artifact = decode(Path(path).read_text(), "context artifact")
    if (not isinstance(artifact, dict) or set(artifact) != {
            "schema_version", "question_id", "manifest_sha256", "records"}
            or type(artifact["schema_version"]) is not int or artifact["schema_version"] != 2
            or artifact["question_id"] != trace.get("question_id")
            or artifact["manifest_sha256"] != manifest_hash
            or trace.get("manifest_sha256") != manifest_hash
            or trace.get("status") != "retrieval_complete"
            or canonical_hash(artifact) != trace.get("context_sha256")):
        fail("Context artifact scope, content or manifest mismatch")
    context_records_v2(artifact["records"], trace["retrieved_ids"])
    return artifact


def map_rejections(found, try_map_id):
    """Map engine `trace_metadata.rejections` to record ids; drop unmapped ids.

    `try_map_id(engine_id)` returns the record id or None. Entries must be
    exactly `{chunk_id, stage, reason}` with string values; anything else
    fails the search rather than rendering a malformed diagnostic.
    """
    found_list = found.get("trace_metadata", {}).get("rejections")
    if found_list is None:
        return []
    if not isinstance(found_list, list):
        fail("Engine rejections must be a list")
    mapped = []
    for item in found_list:
        if (not isinstance(item, dict) or set(item) != REJECTION_KEYS
                or not all(isinstance(item[key], str) and item[key] for key in REJECTION_KEYS)):
            fail("Each engine rejection needs string chunk_id, stage and reason")
        record_id = try_map_id(item["chunk_id"])
        if record_id is None:
            continue
        mapped.append({"chunk_id": record_id, "stage": item["stage"], "reason": item["reason"]})
    return mapped


def checked_rejections(value):
    """Validate a mapped rejections list (the shape `stage_diagnostics` reads)."""
    if not isinstance(value, list):
        fail("rejections must be a list")
    for item in value:
        if (not isinstance(item, dict) or set(item) != REJECTION_KEYS
                or not all(isinstance(item[key], str) and item[key] for key in REJECTION_KEYS)):
            fail("Each rejection needs string chunk_id, stage and reason")
    return [dict(item) for item in value]


def trace_row_v2(found, row, manifest_hash):
    """Build one v2 sidecar row from a completed search result.

    `evidence` is the unchanged v1 trace projection (`coverage` + `stages`,
    the key v1 rows use and `stage_diagnostics` reads); `rejections` is the
    mapped top-level list. The row carries IDs only: no text, query,
    exception body or answer.
    """
    ids = id_list(found.get("retrieved_ids"))
    return {"question_id": row["question_id"], "manifest_sha256": manifest_hash,
            "status": "retrieval_complete", "retrieved_ids": ids,
            "evidence": trace_projection(found),
            "rejections": checked_rejections(found.get("rejections", []))}


def failed_trace_row_v2(row, manifest_hash, failure_stage):
    return {"question_id": row["question_id"], "manifest_sha256": manifest_hash,
            "status": "failed", "failure_stage": failure_stage, "rejections": []}


def write_trace_row_v2(handle, trace_row):
    """Append one v2 sidecar row as a JSON line.

    Row shape: `{question_id, manifest_sha256, status, retrieved_ids,
    evidence, rejections}` (a failed row has `failure_stage` instead of
    `retrieved_ids`/`evidence`). `stage_diagnostics.py --trace` reads it
    directly. The v1 `runner.py --resume` does not accept this file (unknown
    key `rejections`); it is a separate sidecar.
    """
    handle.write(json.dumps(trace_row, allow_nan=False) + "\n")
    handle.flush()


class LocalEngineAdapterV2(LocalEngineAdapter):
    """Same owned engine lifecycle; additionally exposes v2 evidence records."""

    def map_id(self, engine_id):
        """Engine chunk id -> record id; KeyError for an id this question never wrote."""
        return self.mapping[engine_id]

    def try_map_id(self, engine_id):
        try:
            return self.map_id(engine_id)
        except KeyError:
            return None

    def reverse_map_id(self, record_id):
        """Record id -> engine chunk id; fails for an id outside this question."""
        reverse = {value: key for key, value in self.mapping.items()}
        if len(reverse) != len(self.mapping) or record_id not in reverse:
            fail("Requested neighbor ID outside current question")
        return reverse[record_id]

    def request(self, path, payload=None):
        if path != "/search":
            return super().request(path, payload)
        if self.process is None or self.process.poll() is not None:
            fail("Owned engine unavailable")
        # Same wire call as the frozen v1 branch: trace on, dry_run forced so
        # benchmark arms never boost importance/access state for later arms.
        search = {key: payload[key] for key in ("query", "top_k", "trace")}
        search["dry_run"] = True
        found = self.engine_request("/v1/search", search)
        ids = [self.map_id(hit["id"]) for hit in found["results"]]
        trace = found.get("trace_metadata", {})
        for stage in trace.get("evidence", {}).get("stages", []):
            stage["chunk_ids"] = [self.map_id(item) for item in stage["chunk_ids"]]
        rejections = map_rejections(found, self.try_map_id)
        result = {"retrieved_ids": ids, "trace_metadata": trace, "rejections": rejections}
        if payload.get("capture_context") is True:
            result["retrieved_records"] = [
                {"record_id": self.map_id(hit["id"]), "content": hit["content"]}
                for hit in found["results"]]
            result["retrieved_records_v2"] = [
                from_engine_hit(hit, self.map_id(hit["id"])) for hit in found["results"]]
        return result

    def fetch_records_v2(self, record_ids):
        """Read stored evidence for scoped neighbor IDs, in order, as v2 records."""
        if self.settings.get("capture_context") is not True:
            fail("Fetching text requires explicit context capture opt-in")
        if self.process is None or self.process.poll() is not None:
            fail("Owned engine unavailable")
        ids = id_list(record_ids)
        engine_ids = [self.reverse_map_id(record_id) for record_id in ids]
        records = []
        for record_id, engine_id in zip(ids, engine_ids):
            item = self.engine_request("/api/memories/" + quote(engine_id, safe=""))
            if item.get("id") != engine_id:
                fail("Stored evidence identity mismatch")
            records.append(from_memory_item(item, record_id))
        return context_records_v2(records, ids)
