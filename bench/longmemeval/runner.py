#!/usr/bin/env python3
"""Local, retrieval-only benchmark adapter runner (no reader or judge)."""
import argparse
from datetime import datetime, timezone
import ipaddress
import json
import re
import sys
from pathlib import Path
from urllib.parse import urlsplit
from urllib.request import Request, build_opener, ProxyHandler, HTTPRedirectHandler

from validate import canonical_hash, check_manifest, decode, fail, sha256


OPERATION_POLICIES = {"E0", "E1-D", "E1-F", "E1-C"}
DATE_AWARE_POLICIES = {"E1-D", "E1-C"}
QUESTION_DATE = re.compile(
    r"^(\d{4})/(\d{2})/(\d{2}) \((Mon|Tue|Wed|Thu|Fri|Sat|Sun)\) (\d{2}):(\d{2})$"
)
WEEKDAYS = ("Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun")


class NoRedirect(HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        fail("Adapter redirects are forbidden")


def local_endpoint(value):
    parsed = urlsplit(value)
    try:
        local = ipaddress.ip_address(parsed.hostname).is_loopback
        port = parsed.port
    except (ValueError, TypeError):
        local, port = False, None
    if (parsed.scheme != "http" or not local or not port or parsed.username
            or parsed.password or parsed.path not in ("", "/")
            or parsed.query or parsed.fragment):
        fail("Adapter must be literal loopback HTTP with explicit port and no path")
    return value.rstrip("/")


def settings_from(config):
    settings = validated_runner_settings(config)
    if settings.get("input_normalization") not in (None, "occurrence-v1"):
        fail("Unknown input normalization")
    if "capture_context" in settings and type(settings["capture_context"]) is not bool:
        fail("capture_context must be a JSON boolean")
    local_endpoint(settings["endpoint"])
    for key in ("instance_id", "run_id", "data_dir"):
        if not isinstance(settings[key], str) or not settings[key].strip():
            fail("Runner identity fields must be nonempty strings")
    if not Path(settings["data_dir"]).is_absolute():
        fail("Runner data_dir must be absolute")
    if type(settings["top_k"]) is not int or settings["top_k"] <= 0:
        fail("Runner top_k must be positive integer")
    if settings["mode"] not in ("raw", "facts", "dual"):
        fail("Runner mode must be raw/facts/dual")
    if settings["mode"] != "raw":
        if sha256(settings.get("facts_path", "")) != settings.get("facts_sha256"):
            fail("External facts artifact hash mismatch")
        settings["_facts"] = decode(Path(settings["facts_path"]).read_text(), "facts")
        if not isinstance(settings["_facts"], dict):
            fail("Facts must map question IDs to explicit lists")
    elif "facts_path" in settings or "facts_sha256" in settings:
        fail("Raw mode does not accept facts")
    if "local_engine" in settings:
        from runner_local import verify_artifacts
        verify_artifacts(settings)
    return settings


def validated_runner_settings(config):
    settings = decode(Path(config).read_text(), "config").get("runner")
    required = {"endpoint", "instance_id", "data_dir", "run_id", "top_k", "mode"}
    optional = {"local_engine", "facts_path", "facts_sha256", "capture_context",
                "input_normalization", "operation_policy", "debug_operation"}
    if (not isinstance(settings, dict) or not required.issubset(settings)
            or set(settings) - required - optional):
        fail("Config runner must contain exactly endpoint/instance_id/data_dir/run_id/top_k/mode")
    validate_operation_settings(settings)
    return settings


def validate_operation_settings(settings):
    if settings.get("operation_policy") not in (None, *OPERATION_POLICIES):
        fail("Unknown operation policy")
    if "debug_operation" in settings and type(settings["debug_operation"]) is not bool:
        fail("debug_operation must be a JSON boolean")


class Adapter:
    def __init__(self, settings):
        self.settings = settings
        self.endpoint = local_endpoint(settings["endpoint"])
        self.opener = build_opener(ProxyHandler({}), NoRedirect())

    def request(self, path, payload=None):
        body = None if payload is None else json.dumps(payload, allow_nan=False).encode()
        request = Request(self.endpoint + path, data=body,
                          headers={"Content-Type": "application/json"})
        with self.opener.open(request, timeout=10) as response:
            if response.status != 200:
                fail("Adapter requires HTTP 200")
            result = decode(response.read(8 * 1024 * 1024 + 1).decode(), "adapter")
        if not isinstance(result, dict):
            fail("Adapter response must be an object")
        return result

    def close(self):
        pass

    def identity(self):
        result = self.request("/identity")
        expected = {key: self.settings[key] for key in ("instance_id", "data_dir", "run_id")}
        expected.update({"schema_version": 1, "dedicated": True, "network_policy": "offline",
                         "paid_calls_enabled": False})
        if canonical_hash(result) != canonical_hash(expected):
            fail("Adapter identity/offline attestation mismatch")


def question_payload(row, settings=None):
    """Strip all evaluation annotations before the representation adapter."""
    from representations import build_records
    clean = {key: row[key] for key in ("question_id", "haystack_session_ids", "haystack_dates")}
    clean["haystack_sessions"] = [
        [{key: turn[key] for key in ("role", "content")} for turn in session]
        for session in row["haystack_sessions"]]
    mode = settings["mode"] if settings else "raw"
    facts = settings["_facts"][row["question_id"]] if mode != "raw" else None
    normalization = settings.get("input_normalization") if settings else None
    records = build_records(clean, mode, facts, input_normalization=normalization)
    question = row.get("question")
    if not isinstance(question, str) or not question.strip() or not records:
        fail("Question and source records must be nonempty")
    return records, question


def reference_time(row):
    """Convert the frozen question clock without reading labels or host time."""
    value = row.get("question_date")
    match = QUESTION_DATE.fullmatch(value) if isinstance(value, str) else None
    if match is None:
        return None
    try:
        stamp = datetime(*(int(match.group(i)) for i in (1, 2, 3, 5, 6)),
                         tzinfo=timezone.utc)
    except ValueError:
        return None
    if WEEKDAYS[stamp.weekday()] != match.group(4):
        return None
    return stamp.isoformat().replace("+00:00", "Z")


def operation_fields(row, settings):
    """Return opt-in E1 wire fields and their text-free trace binding."""
    policy = settings.get("operation_policy")
    if policy is None:
        return {}, {}
    wire = {"operation_policy": policy}
    trace = {"operation_policy": policy}
    if settings.get("debug_operation", False):
        wire["debug_operation"] = True
    if policy in DATE_AWARE_POLICIES:
        anchor = reference_time(row)
        trace["reference_time_sha256"] = canonical_hash(anchor)
        if anchor is not None:
            wire["reference_time"] = anchor
    return wire, trace


def verify_operation(record, row, settings):
    _, expected = operation_fields(row, settings)
    actual = {key: record[key] for key in (
        "operation_policy", "reference_time_sha256") if key in record}
    if canonical_hash(actual) != canonical_hash(expected):
        fail("Resume operation identity differs from frozen input")


def operation_search(row, settings, result, scope, query):
    wire, trace = operation_fields(row, settings)
    result.update(trace)
    search = {"scope": scope, "query": query, "top_k": settings["top_k"], "trace": True}
    search.update(wire)
    return search


def verify_resume_identity(record, row, settings, manifest_hash):
    verify_normalization(record, row, settings, manifest_hash)
    verify_operation(record, row, settings)


def resume_fields():
    return {"question_id", "manifest_sha256", "status", "failure_stage", "ingested_ids",
            "retrieved_ids", "evidence", "context_sha256", "normalization_report",
            "normalization_sha256", "operation_policy", "reference_time_sha256"}


def id_list(value):
    if not isinstance(value, list) or any(not isinstance(item, str) or not item for item in value):
        fail("Expected list of nonempty IDs")
    if len(value) != len(set(value)):
        fail("Duplicate IDs")
    return value


def trace_projection(result):
    evidence = result.get("trace_metadata", {}).get("evidence")
    if evidence is None:
        return {"coverage": "unavailable", "stages": []}
    if type(evidence.get("schema_version")) is not int or evidence.get("schema_version") != 1 or evidence.get("coverage") != "post_rank_to_response":
        fail("Unknown evidence schema")
    allowed = ["ranked_candidates", "after_soft_delete", "after_filters_and_dedup", "response"]
    if [stage.get("stage") for stage in evidence.get("stages", [])] != allowed:
        fail("Missing, duplicate or out-of-order evidence stages")
    stages = []
    for stage in evidence.get("stages", []):
        if stage.get("stage") not in allowed:
            fail("Unknown evidence stage")
        stages.append({"stage": stage["stage"], "chunk_ids": id_list(stage["chunk_ids"])})
    return {"coverage": evidence["coverage"], "stages": stages}


def check_evidence(evidence, expected, retrieved):
    if evidence["coverage"] == "unavailable":
        return
    for stage in evidence["stages"]:
        if set(stage["chunk_ids"]) - set(expected):
            fail("Evidence IDs outside question scope")
    if evidence["stages"][-1]["chunk_ids"] != retrieved:
        fail("Response evidence does not match retrieval order")


def context_records(value, ids):
    """Validate exact returned evidence; never fill fields from source records."""
    if not isinstance(value, list):
        fail("Missing actual returned context records")
    for record in value:
        if (not isinstance(record, dict) or set(record) != {"record_id", "content"}
                or not isinstance(record["content"], str)):
            fail("Context records require only record_id and actual content")
    if id_list([record["record_id"] for record in value]) != ids:
        fail("Actual context does not preserve returned IDs and rank")
    return value


def context_artifact(found, row, manifest_hash, ids):
    return {"schema_version": 1, "question_id": row["question_id"],
            "manifest_sha256": manifest_hash,
            "records": context_records(found.get("retrieved_records"), ids)}


def save_context_artifact(directory, artifact):
    """Write immutable content-addressed evidence only on explicit opt-in."""
    digest = canonical_hash(artifact)
    path = Path(directory) / (digest + ".json")
    if path.exists():
        if canonical_hash(decode(path.read_text(), "context artifact")) != digest:
            fail("Existing context artifact is corrupt")
        return
    with path.open("x") as handle:
        handle.write(json.dumps(artifact, ensure_ascii=False, allow_nan=False) + "\n")


def load_context_artifact(path, trace, manifest_hash):
    """Verify immutable context against its manifest-bound ID-only trace."""
    artifact = decode(Path(path).read_text(), "context artifact")
    if (not isinstance(artifact, dict) or set(artifact) != {
            "schema_version", "question_id", "manifest_sha256", "records"}
            or type(artifact["schema_version"]) is not int or artifact["schema_version"] != 1
            or artifact["question_id"] != trace.get("question_id")
            or artifact["manifest_sha256"] != manifest_hash
            or trace.get("manifest_sha256") != manifest_hash
            or trace.get("status") != "retrieval_complete"
            or canonical_hash(artifact) != trace.get("context_sha256")):
        fail("Context artifact scope, content or manifest mismatch")
    context_records(artifact["records"], trace["retrieved_ids"])
    return artifact


def normalization_fields(row, settings, manifest_hash):
    """Automatically bind the text-free normalization ledger to each question."""
    if settings.get("input_normalization") != "occurrence-v1":
        return {}
    from representations import normalization_report
    report = {**normalization_report(row), "manifest_sha256": manifest_hash}
    return {"normalization_report": report, "normalization_sha256": canonical_hash(report)}


def verify_normalization(record, row, settings, manifest_hash):
    expected = normalization_fields(row, settings, manifest_hash)
    actual = {key: record[key] for key in ("normalization_report", "normalization_sha256")
              if key in record}
    # Recompute from the frozen input, not just an attacker-supplied report/hash pair.
    if canonical_hash(actual) != canonical_hash(expected):
        fail("Resume normalization ledger missing or differs from frozen input")


def retrieve(adapter, row, manifest_hash, *, context_sink=None):
    result = {"question_id": row["question_id"], "manifest_sha256": manifest_hash,
              "status": "failed", "failure_stage": "prepare"}
    try:
        result.update(normalization_fields(row, adapter.settings, manifest_hash))
        capture = adapter.settings.get("capture_context", False)
        if capture != (context_sink is not None):
            fail("Context capture requires both manifest opt-in and an explicit sink")
        records, query = question_payload(row, adapter.settings)
        scope = canonical_hash([manifest_hash, row["question_id"]])
        expected = [record["record_id"] for record in records]
        result["failure_stage"] = "identity"
        adapter.identity()
        result["failure_stage"] = "ingest"
        ingested = adapter.request("/ingest", {"scope": scope, "records": records})
        accepted = id_list(ingested.get("accepted_ids"))
        if ingested.get("status") != "complete" or set(accepted) != set(expected):
            fail("Partial ingest")
        result["ingested_ids"] = accepted
        result["failure_stage"] = "readiness"
        ready = adapter.request("/ready", {"scope": scope, "expected_ids": expected})
        if (ready.get("ready") is not True or type(ready.get("pending_jobs")) is not int
                or ready["pending_jobs"] != 0 or ready.get("indexes_ready") is not True
                or set(id_list(ready.get("indexed_ids"))) != set(expected)):
            fail("Readiness unproven")
        result["failure_stage"] = "retrieval"
        search = operation_search(row, adapter.settings, result, scope, query)
        if capture:
            search["capture_context"] = True
        found = adapter.request("/search", search)
        ids = id_list(found.get("retrieved_ids"))
        if not set(ids).issubset(expected) or len(ids) > adapter.settings["top_k"]:
            fail("Retrieved IDs outside question scope or top_k")
        evidence = trace_projection(found)
        check_evidence(evidence, expected, ids)
        if capture:
            artifact = context_artifact(found, row, manifest_hash, ids)
            digest = canonical_hash(artifact)
            context_sink(artifact)
            result["context_sha256"] = digest
        result.update({"status": "retrieval_complete", "retrieved_ids": ids,
                       "evidence": evidence})
        result.pop("failure_stage")
    except (OSError, ValueError, KeyError, TypeError, AttributeError):
        # Never persist exception text, request content, response bodies or gold answers.
        pass
    finally:
        adapter.close()
    return result


def load_resume(path, rows, manifest_hash, settings):
    if not path.exists():
        return {}
    index, seen = {row["question_id"] for row in rows}, {}
    allowed = resume_fields()
    for line in path.read_text().splitlines():
        record = decode(line, "trace sidecar")
        qid = record.get("question_id")
        if (qid not in index or qid in seen or set(record) - allowed
                or record.get("manifest_sha256") != manifest_hash
                or record.get("status") not in ("failed", "retrieval_complete")):
            fail("Invalid or mismatched resume sidecar")
        if record["status"] == "failed" and record.get("failure_stage") not in (
                "prepare", "identity", "ingest", "readiness", "retrieval"):
            fail("Invalid failure stage")
        if record["status"] == "failed" and ({"retrieved_ids", "evidence", "context_sha256"} & set(record)):
            fail("Failed resume rows cannot carry retrieval evidence")
        if record["status"] == "retrieval_complete":
            capture = settings.get("capture_context", False)
            digest = record.get("context_sha256")
            if capture != (digest is not None):
                fail("Resume context capture differs from frozen configuration")
            if capture and (not isinstance(digest, str) or len(digest) != 64
                            or any(char not in "0123456789abcdef" for char in digest)):
                fail("Invalid context artifact digest")
            id_list(record.get("retrieved_ids"))
            if "failure_stage" in record:
                fail("Completed retrieval cannot carry failure stage")
        source = next(row for row in rows if row["question_id"] == qid)
        verify_resume_identity(record, source, settings, manifest_hash)
        expected = {item["record_id"] for item in question_payload(source, settings)[0]}
        if "ingested_ids" in record and set(id_list(record["ingested_ids"])) != expected:
            fail("Resume ingest IDs mismatch")
        if record["status"] == "retrieval_complete":
            if set(record["retrieved_ids"]) - expected or len(record["retrieved_ids"]) > settings["top_k"]:
                fail("Resume retrieval IDs mismatch")
            if "ingested_ids" not in record or "evidence" not in record:
                fail("Resume missing stage evidence")
            evidence = record["evidence"]
            if evidence != {"coverage": "unavailable", "stages": []}:
                checked = trace_projection({"trace_metadata": {"evidence": {"schema_version": 1, **evidence}}})
                if checked != evidence:
                    fail("Resume evidence mismatch")
                check_evidence(evidence, expected, record["retrieved_ids"])
        seen[qid] = record
    return seen


def run(args):
    manifest_hash = check_manifest(args.manifest, args.dataset, args.config)
    settings = settings_from(args.config)
    contexts = getattr(args, "contexts", None)
    if settings.get("capture_context", False) != (contexts is not None):
        fail("--contexts requires frozen runner.capture_context=true and vice versa")
    rows = decode(args.dataset.read_text(), "dataset")
    # Validate every projection before the first network call or write.
    for row in rows:
        question_payload(row, settings)
    if args.dry_run:
        return {"mode": "dry_run", "expected": len(rows), "network_calls": 0,
                "manifest_sha256": manifest_hash}
    if args.trace.exists() and not args.resume:
        fail("Existing trace requires --resume")
    seen = load_resume(args.trace, rows, manifest_hash, settings)
    if contexts is not None:
        for record in seen.values():
            if record["status"] == "retrieval_complete":
                load_context_artifact(Path(contexts) / (record["context_sha256"] + ".json"),
                                      record, manifest_hash)
        Path(contexts).mkdir(parents=True, exist_ok=True)
    if "local_engine" in settings:
        from runner_local import LocalEngineAdapter
        adapter = LocalEngineAdapter(settings)
    else:
        adapter = Adapter(settings)
    with args.trace.open("a" if args.resume else "x") as handle:
        for row in rows:
            if row["question_id"] in seen:
                continue
            sink = None if contexts is None else lambda artifact: save_context_artifact(contexts, artifact)
            record = retrieve(adapter, row, manifest_hash, context_sink=sink)
            handle.write(json.dumps(record, allow_nan=False) + "\n")
            handle.flush()
            seen[row["question_id"]] = record
    return {"mode": "retrieval_only", "expected": len(rows), "observed": len(seen),
            "failed": sum(record["status"] == "failed" for record in seen.values()),
            "missing_ids": sorted({row["question_id"] for row in rows} - set(seen)),
            "manifest_sha256": manifest_hash, "accuracy": None}


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("dataset", "config", "manifest", "trace"):
        parser.add_argument("--" + name, required=True, type=Path)
    parser.add_argument("--contexts", type=Path, help="Explicit directory for actual returned evidence")
    parser.add_argument("--dry-run", action="store_true")
    parser.add_argument("--resume", action="store_true")
    args = parser.parse_args(argv)
    try:
        summary = run(args)
    except (OSError, ValueError, KeyError, TypeError) as exc:
        print("Runner rejected artifacts: " + type(exc).__name__, file=sys.stderr)
        return 2
    print(json.dumps(summary, indent=2))
    return 2 if summary.get("failed") or summary.get("missing_ids") else 0


if __name__ == "__main__":
    sys.exit(main())
