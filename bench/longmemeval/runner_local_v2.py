"""Owned local adapter extension that also captures evidence contract v2.

`runner_local.LocalEngineAdapter` is frozen: this subclass changes none of its
behaviour. It overrides only the `/search` branch, repeating the v1 wire call
and v1 result shape verbatim (the equality is pinned by tests). When
`capture_context` is on, the result additionally carries
`retrieved_records_v2`, the same hits mapped through
`evidence_v2.from_engine_hit`, next to the unchanged v1 `retrieved_records`
that `runner.retrieve` keeps validating. `fetch_records_v2` is the neighbor
counterpart over `GET /api/memories/<id>`; that endpoint exposes no
`evidence` object today, so its records carry null metadata until the engine
adds the same object it attaches to search hits. Text is never reconstructed:
every `content` is the string the engine returned.
"""
from pathlib import Path
from urllib.parse import quote

from evidence_v2 import from_engine_hit, from_memory_item
from qa_context_v2 import validate_records_v2
from runner import id_list
from runner_local import LocalEngineAdapter
from validate import canonical_hash, decode, fail


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


class LocalEngineAdapterV2(LocalEngineAdapter):
    """Same owned engine lifecycle; additionally exposes v2 evidence records."""

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
        ids = [self.mapping[hit["id"]] for hit in found["results"]]
        trace = found.get("trace_metadata", {})
        for stage in trace.get("evidence", {}).get("stages", []):
            stage["chunk_ids"] = [self.mapping[item] for item in stage["chunk_ids"]]
        result = {"retrieved_ids": ids, "trace_metadata": trace}
        if payload.get("capture_context") is True:
            result["retrieved_records"] = [
                {"record_id": self.mapping[hit["id"]], "content": hit["content"]}
                for hit in found["results"]]
            result["retrieved_records_v2"] = [
                from_engine_hit(hit, self.mapping[hit["id"]]) for hit in found["results"]]
        return result

    def fetch_records_v2(self, record_ids):
        """Read stored evidence for scoped neighbor IDs, in order, as v2 records."""
        if self.settings.get("capture_context") is not True:
            fail("Fetching text requires explicit context capture opt-in")
        if self.process is None or self.process.poll() is not None:
            fail("Owned engine unavailable")
        ids = id_list(record_ids)
        reverse = {record_id: engine_id for engine_id, record_id in self.mapping.items()}
        if len(reverse) != len(self.mapping) or set(ids) - set(reverse):
            fail("Requested neighbor ID outside current question")
        records = []
        for record_id in ids:
            engine_id = reverse[record_id]
            item = self.engine_request("/api/memories/" + quote(engine_id, safe=""))
            if item.get("id") != engine_id:
                fail("Stored evidence identity mismatch")
            records.append(from_memory_item(item, record_id))
        return context_records_v2(records, ids)
