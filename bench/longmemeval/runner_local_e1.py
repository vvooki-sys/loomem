#!/usr/bin/env python3
"""E1 fragments arm: ingest each session as ONE `memory_ingest` MCP call.

The E0 control arm (`runner_local.ingest`) stores one chunk per turn through
`POST /v1/store`. This arm hands the owned engine the whole session once, via
its MCP endpoint (`POST /mcp`, JSON-RPC 2.0, legacy session path: one
`initialize` per engine process, then `tools/call` with the returned
`Mcp-Session-Id`; the same Bearer token as `runner_local.engine_request`
authenticates and selects the stream). The engine persists the canonical
source and, with `[fragments] enabled = true` in the generated config, one
fragment chunk per transcript turn (long turns split further). Knowledge
extraction stays disabled, keyless and offline: the generated config is the
frozen v1 template with `[fragments]` switched on (plus the two adapter knobs
`provenance_role` / `token_counter`) and `[search] evidence_fields = true` so
search hits carry the v2 `evidence` object.

Transcript format (what `loomem_core::fragments` parses, see its `turns.rs`):
one line per turn, `<role>: <content>`, lines joined by a single `\\n`, no
trailing newline, roles exactly as the dataset gives them (`user` /
`assistant`, the only two accepted). A turn whose content contains newlines
continues on the following lines; the engine treats a continuation line as
part of the preceding turn unless it itself starts with a recognised role
marker (`user:`, `assistant:`, `human:`, `ai:`, `system:` and their bold /
bracketed / dash forms), in which case the engine splits there.

Id scheme: a session record is a v1 `representations._record` of kind
`session` whose content is the rendered transcript, whose sources list every
turn and whose `date` is the dataset date verbatim (so the record id binds the
dataset input, like v1). The engine source id maps to that record id; an
engine fragment chunk `<source uuid>:f<k>` maps to `<session record id>:f<k>`.
Fragment indices are the engine's: whitespace-only fragments are skipped at
ingest, so indices may have gaps. Only exact IDs in the validated structured
ingest receipt are admitted; a fragment count never establishes membership.

Readiness requires exact per-ID evidence from the evaluation-only endpoint.
The adapter seals ingestion before probing, on its fresh isolated engine with
pinned local model/config. Missing or malformed proof fails before retrieval.

Dates: `conversation_date` is `longmemeval_dates.to_iso(session date)`; a
date that does not normalise is not sent (the engine then stores no
conversation date and the reader sees null, exactly as the E0 arm would).

This module is wired into nothing by default. It is a separate driver with
its own v2 sidecar (`runner_local_v2.write_trace_row_v2`); `runner.py
--resume` does not read that sidecar and E1 runs are create-only.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import secrets
import socket
import subprocess
import sys
import time
from urllib.request import Request

from ingest_receipt import ReceiptError, expected_ids_from_mcp_result
from longmemeval_dates import to_iso
from readiness_artifact import build_artifact, save_artifact
from readiness_proof import EVIDENCE as READINESS_EVIDENCE, validate_proof
from representations import _record, _session_inputs, _turn_source
from runner import id_list, operation_search, settings_from, trace_projection
from runner_local import effective_config, replace_setting
from runner_local_v2 import (LocalEngineAdapterV2, context_artifact_v2, failed_trace_row_v2,
                             trace_row_v2, write_trace_row_v2)
from validate import canonical_hash, check_manifest, decode, fail

TRANSCRIPT_ROLES = ("user", "assistant")
PROVENANCE_ROLES = ("claim", "evidence", "cue")
TOKEN_COUNTERS = ("approx", "tiktoken", "embedder")
E1_SETTINGS_KEYS = frozenset({"provenance_role", "token_counter"})
UUID = r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}"
INGEST_RESULT = re.compile(
    r"^Stored source transcript \(id: (" + UUID + r")\)\. Knowledge extraction disabled\. "
    r"Stored (0|[1-9][0-9]*) fragments\.$")
FRAGMENT_ID = re.compile(r"^(" + UUID + r"):f(0|[1-9][0-9]*)$")
RECORD_FRAGMENT_ID = re.compile(r"^([0-9a-f]{64}):f(0|[1-9][0-9]*)$")
MCP_CLIENT = {"name": "loomem-longmemeval-e1", "version": "1"}


def render_transcript(session):
    """`role: content` per turn, joined by a single newline; roles verbatim.

    Roles are restricted to the dataset vocabulary (`user`, `assistant`),
    which the engine's turn parser recognises as line markers; any other role
    would silently merge turns on the engine side, so it is rejected here.
    """
    if not isinstance(session, list) or not session:
        raise ValueError("each session must be a nonempty list of turns")
    lines = []
    for turn in session:
        role, content = turn.get("role"), turn.get("content")
        if role not in TRANSCRIPT_ROLES:
            raise ValueError("turn role must be user or assistant")
        if not isinstance(content, str):
            raise ValueError("turn content must be a string")
        lines.append(role + ": " + content)
    return "\n".join(lines)


def session_records(question, input_normalization=None):
    """One label-blind record per session: kind `session`, transcript content.

    Built from the same v1 helpers as `representations._raw_records`, so the
    record id is the v1 `_record` hash of `[question_id, record]`. Under
    `occurrence-v1`, exactly-empty turns are excluded from the transcript and
    from the sources (as the v1 ledger excludes them); repeated session ids
    carry `session_occurrence`. The `date` is the dataset string verbatim.
    """
    question_id = question["question_id"]
    sessions, ids, dates = _session_inputs(question, input_normalization)
    counts, occurrences, records = {}, {}, []
    for session_id in ids:
        counts[session_id] = counts.get(session_id, 0) + 1
    for session, session_id, date in zip(sessions, ids, dates):
        occurrence = occurrences.get(session_id, 0)
        occurrences[session_id] = occurrence + 1
        if not isinstance(session, list):
            raise ValueError("each session must be a list of turns")
        kept, sources = [], []
        for index, turn in enumerate(session):
            if input_normalization == "occurrence-v1" and turn.get("content") == "":
                continue
            kept.append({"role": turn.get("role"), "content": turn.get("content")})
            sources.append(_turn_source(session_id, index, occurrence, counts[session_id] > 1))
        records.append(_record(question_id, "session", render_transcript(kept), sources,
                               {"role": None, "date": date}))
    return records


def question_payload_e1(row, settings):
    """Strip evaluation annotations; return (session records, question text)."""
    clean = {key: row[key] for key in ("question_id", "haystack_session_ids", "haystack_dates")}
    clean["haystack_sessions"] = [
        [{key: turn[key] for key in ("role", "content")} for turn in session]
        for session in row["haystack_sessions"]]
    records = session_records(clean, settings.get("input_normalization"))
    question = row.get("question")
    if not isinstance(question, str) or not question.strip() or not records:
        fail("Question and session records must be nonempty")
    return records, question


def e1_settings(config):
    """The `runner_e1` object of the frozen config: the two fragment knobs."""
    found = decode(Path(config).read_text(), "config").get("runner_e1")
    if not isinstance(found, dict) or set(found) != E1_SETTINGS_KEYS:
        fail("Config runner_e1 must contain exactly provenance_role and token_counter")
    if found["provenance_role"] not in PROVENANCE_ROLES:
        fail("runner_e1.provenance_role must be claim, evidence or cue")
    if found["token_counter"] not in TOKEN_COUNTERS:
        fail("runner_e1.token_counter must be approx, tiktoken or embedder")
    return dict(found)


def settings_from_e1(config):
    """Frozen runner settings (owned engine, raw mode required) plus E1 knobs."""
    settings = settings_from(config)
    if "local_engine" not in settings or settings["mode"] != "raw":
        fail("E1 requires an owned local engine in raw mode")
    settings["_e1"] = e1_settings(config)
    return settings


def effective_config_e1(settings, work):
    """v1 generated config plus fragments on, the two knobs and evidence fields."""
    text = effective_config(settings, work)
    knobs = settings["_e1"]
    overrides = [("fragments", "enabled", True),
                 ("fragments", "provenance_role", knobs["provenance_role"]),
                 ("fragments", "token_counter", knobs["token_counter"]),
                 ("search", "evidence_fields", True)]
    for section, key, value in overrides:
        text = replace_setting(text, section, key, value)
    return text


def parse_ingest_result(result):
    """Validate receipt and legacy text -> (source id, exact expected IDs)."""
    try:
        expected_ids = expected_ids_from_mcp_result(result)
    except ReceiptError as error:
        fail(str(error))
    content = result.get("content")
    if (not isinstance(content, list) or len(content) != 1 or not isinstance(content[0], dict)
            or content[0].get("type") != "text" or not isinstance(content[0].get("text"), str)):
        fail("memory_ingest result must be one text block")
    match = INGEST_RESULT.fullmatch(content[0]["text"])
    if match is None:
        fail("memory_ingest result text is not the expected extraction-disabled form")
    source_id = result["structuredContent"]["ingest_receipt"]["source_id"]
    if match.group(1) != source_id or int(match.group(2)) != len(expected_ids) - 1:
        fail("Legacy ingest text disagrees with structured receipt")
    return source_id, expected_ids


def fragment_record_id(session_record_id, index):
    return session_record_id + ":f" + str(index)


def ingest_arguments(record):
    """`memory_ingest` arguments; the date key is absent when it does not normalise."""
    arguments = {"content": record["content"]}
    date = to_iso(record["date"])
    if date is not None:
        arguments["conversation_date"] = date
    return arguments


class LocalEngineAdapterE1(LocalEngineAdapterV2):
    """Owned engine with fragments on; sessions ingested through MCP."""

    def __init__(self, settings):
        super().__init__(settings)
        if "_e1" not in settings:
            fail("E1 adapter requires settings from settings_from_e1")
        self.expected_ids_by_source = {}
        self.ingest_sources = {}
        self.sealed = False
        self.readiness_observations = {}
        self.effective_config_sha256 = None
        self.session_id = None
        self.rpc_id = 0

    # -- MCP transport -----------------------------------------------------

    def mcp_request(self, body, session_id=None):
        """POST one JSON-RPC body to `/mcp`; return (response JSON, session header)."""
        headers = {"Content-Type": "application/json", "Accept": "application/json",
                   "Authorization": "Bearer " + self.token}
        if session_id is not None:
            headers["Mcp-Session-Id"] = session_id
        request = Request(self.endpoint + "/mcp", data=json.dumps(body).encode(), headers=headers)
        with self.opener.open(request, timeout=120) as response:
            if response.status != 200:
                fail("Engine MCP endpoint requires HTTP 200")
            return json.load(response), response.headers.get("Mcp-Session-Id")

    def rpc(self, method, params):
        self.rpc_id += 1
        body = {"jsonrpc": "2.0", "id": self.rpc_id, "method": method, "params": params}
        found, session = self.mcp_request(body, self.session_id)
        if (not isinstance(found, dict) or found.get("jsonrpc") != "2.0"
                or found.get("id") != self.rpc_id or "error" in found
                or not isinstance(found.get("result"), dict)):
            fail("Engine JSON-RPC response is not a success for this request")
        return found["result"], session

    def mcp_initialize(self):
        params = {"protocolVersion": "2025-03-26", "capabilities": {}, "clientInfo": MCP_CLIENT}
        result, session = self.rpc("initialize", params)
        if not isinstance(session, str) or not session or "protocolVersion" not in result:
            fail("Engine MCP initialize did not open a session")
        self.session_id = session

    def tool_call(self, name, arguments):
        if self.session_id is None:
            self.mcp_initialize()
        result, _ = self.rpc("tools/call", {"name": name, "arguments": arguments})
        return result

    # -- lifecycle ---------------------------------------------------------

    def identity(self):
        """Fresh work directory, E1 config and owned process (v1 steps, E1 config)."""
        self.close()
        self.session_id = None
        self.expected_ids_by_source = {}
        self.ingest_sources = {}
        self.sealed = False
        self.readiness_observations = {}
        self.effective_config_sha256 = None
        self.counter += 1
        work = self.invocation / str(self.counter)
        work.mkdir(mode=0o700)
        config_text = effective_config_e1(self.settings, work)
        self.effective_config_sha256 = hashlib.sha256(config_text.encode()).hexdigest()
        (work / "config.toml").write_text(config_text)
        (work / "entities.toml").write_text("")
        (work / "synonyms.toml").write_text("synonyms = []\n")
        (work / "pii_blocklist.txt").write_text("")
        self.mapping = {}
        self.token = secrets.token_hex(32)
        self.launch(work)

    def launch(self, work):
        """Start the owned process on a prepared work directory; wait for empty status."""
        # Fail before launching when another listener occupies the requested port.
        with socket.socket() as probe:
            probe.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
            probe.bind(("127.0.0.1", int(self.endpoint.rsplit(":", 1)[1])))
        env = {"PATH": os.defpath, "HOME": str(work), "LOOMEM_CONFIG": str(work / "config.toml"),
               "LOOMEM_AUTH_TOKEN": self.token, "RUST_LOG": "error",
               "LOOMEM_EVALUATION_READINESS": "1"}
        self.process = subprocess.Popen([self.config["binary"]], cwd=work, env=env,
                                        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        deadline = time.monotonic() + 45
        while time.monotonic() < deadline:
            if self.process.poll() is not None:
                fail("Owned engine exited")
            try:
                status = self.engine_request("/v1/status")["config_summary"]
                if status["tantivy_docs"] != 0 or status["embeddings_count"] != 0:
                    fail("Owned database must start empty")
                return
            except OSError:
                time.sleep(0.05)
        fail("Owned engine startup timeout")

    # -- id mapping --------------------------------------------------------

    def map_id(self, engine_id):
        if engine_id in self.mapping:
            return self.mapping[engine_id]
        match = FRAGMENT_ID.fullmatch(engine_id) if isinstance(engine_id, str) else None
        if (match is None or match.group(1) not in self.mapping
                or engine_id not in self.expected_ids_by_source.get(match.group(1), ())):
            raise KeyError(engine_id)
        return fragment_record_id(self.mapping[match.group(1)], int(match.group(2)))

    def reverse_map_id(self, record_id):
        match = RECORD_FRAGMENT_ID.fullmatch(record_id) if isinstance(record_id, str) else None
        if match is None:
            return super().reverse_map_id(record_id)
        source_id = super().reverse_map_id(match.group(1))
        engine_id = source_id + ":f" + match.group(2)
        if engine_id not in self.expected_ids_by_source.get(source_id, ()):
            fail("Fragment ID is absent from ingest receipt")
        return engine_id

    # -- adapter operations ------------------------------------------------

    def request(self, path, payload=None):
        if self.process is None or self.process.poll() is not None:
            fail("Owned engine unavailable")
        if path == "/ingest":
            return self.ingest(payload)
        if path == "/ready":
            return self.ready(payload)
        return super().request(path, payload)

    def ingest(self, payload):
        if self.sealed:
            fail("Ingestion is sealed for readiness verification")
        accepted = []
        for record in payload["records"]:
            if record.get("kind") != "session":
                fail("E1 ingests session records only")
            arguments = ingest_arguments(record)
            result = self.tool_call("memory_ingest", arguments)
            source_id, expected_ids = parse_ingest_result(result)
            if source_id in self.mapping:
                fail("Duplicate engine ID")
            self.mapping[source_id] = record["record_id"]
            self.expected_ids_by_source[source_id] = expected_ids
            self.ingest_sources[source_id] = {
                "source_id": source_id, "content": arguments["content"],
                "conversation_date": arguments.get("conversation_date"),
                "fragment_ids": sorted(expected_ids - {source_id})}
            accepted.append(record["record_id"])
        return {"status": "complete", "accepted_ids": accepted}

    def expected_chunks(self):
        if set(self.mapping) != set(self.expected_ids_by_source):
            fail("Missing expected record set for ingested source")
        return sum(len(ids) for ids in self.expected_ids_by_source.values())

    # Real LongMemEval histories create hundreds of fragments; a flat timeout
    # turns ordinary indexing work into a false readiness failure.
    READY_DEADLINE_SECONDS = 45
    READY_SECONDS_PER_CHUNK = 0.5
    READY_POLL_SECONDS = 0.5

    def ready_deadline_seconds(self, expected):
        return self.READY_DEADLINE_SECONDS + self.READY_SECONDS_PER_CHUNK * expected

    def ready(self, payload):
        self.sealed = True
        if set(id_list(payload["expected_ids"])) != set(self.mapping.values()):
            fail("Readiness expected IDs differ from ingested sessions")
        if set(self.ingest_sources) != set(self.mapping) or not self.mapping:
            fail("Missing source inputs for readiness")
        self.expected_chunks()
        scope = payload["scope"]
        if not isinstance(scope, str) or not scope:
            fail("Readiness requires a scope")
        batches = []
        for source_id, source in self.ingest_sources.items():
            expected = self.expected_ids_by_source[source_id]
            # Matches storage::DEFAULT_STREAM_ID, selected by this owned token.
            request = {"schema_version": 1, "scope": scope,
                       "stream": "__user_default__", "sources": [source]}
            if len(expected) > 1024 or len(json.dumps(request).encode()) > 2 * 1024 * 1024:
                fail("Source exceeds evaluation readiness request limit")
            batches.append((request, expected))
        deadline = time.monotonic() + self.ready_deadline_seconds(self.expected_chunks())
        pending = batches
        while time.monotonic() < deadline:
            complete = True
            still_pending = []
            for request, expected in pending:
                source_id = request["sources"][0]["source_id"]
                observation = self.readiness_observations.setdefault(source_id, {
                    "attempts": 0, "last_proof": None, "failure": None})
                observation["attempts"] += 1
                try:
                    proof = self.engine_request("/v1/evaluation/readiness", request)
                except (OSError, ValueError, KeyError, TypeError):
                    observation["failure"] = "request_failed"
                    raise
                try:
                    proven = validate_proof(proof, scope, expected)
                except (ValueError, KeyError, TypeError):
                    observation["failure"] = "invalid_proof"
                    raise
                observation["last_proof"] = proof
                observation["failure"] = None if proven else "records_not_ready"
                complete &= proven
                if not proven:
                    still_pending.append((request, expected))
            records = []
            for request, _ in batches:
                source_id = request["sources"][0]["source_id"]
                records.extend(self.readiness_observations[source_id]["last_proof"]["records"])
            combined = {"schema_version": 1, "scope": scope, "dimension": 384,
                        "evidence": READINESS_EVIDENCE, "ready": bool(complete),
                        "records": records}
            expected = frozenset().union(*self.expected_ids_by_source.values())
            if validate_proof(combined, scope, expected):
                return combined
            pending = still_pending
            time.sleep(self.READY_POLL_SECONDS)
        fail("Local source+fragment indexing incomplete")


def in_scope_e1(record_id, session_ids):
    """A session record id or `<session record id>:f<k>` of this question."""
    if record_id in session_ids:
        return True
    match = RECORD_FRAGMENT_ID.fullmatch(record_id) if isinstance(record_id, str) else None
    return match is not None and match.group(1) in session_ids


def check_scope_e1(ids, session_ids):
    for record_id in ids:
        if not in_scope_e1(record_id, session_ids):
            fail("Retrieved or traced ID outside question scope")


def retrieve_e1(adapter, row, manifest_hash, *, context_sink=None, readiness_sink=None):
    """Ingest -> readiness -> retrieval for one question; returns a v2 sidecar row.

    Mirrors `runner.retrieve` step for step, with session records as the
    ingest unit and fragment ids admitted in scope checks. Never persists
    exception text, request content, response bodies or gold answers.
    """
    stage = "prepare"
    initialized, success = False, False
    adapter.readiness_observations = {}
    try:
        capture = adapter.settings.get("capture_context", False)
        if readiness_sink is not None and not capture:
            fail("Readiness diagnostics require frozen capture_context opt-in")
        if capture != (context_sink is not None) and not (capture and readiness_sink is not None):
            fail("Context capture requires both manifest opt-in and an explicit sink")
        capture = context_sink is not None
        records, query = question_payload_e1(row, adapter.settings)
        scope = canonical_hash([manifest_hash, row["question_id"]])
        expected = [record["record_id"] for record in records]
        stage = "identity"
        adapter.identity()
        initialized = True
        stage = "ingest"
        ingested = adapter.request("/ingest", {"scope": scope, "records": records})
        accepted = id_list(ingested.get("accepted_ids"))
        if ingested.get("status") != "complete" or set(accepted) != set(expected):
            fail("Partial ingest")
        stage = "readiness"
        ready = adapter.request("/ready", {"scope": scope, "expected_ids": expected})
        exact_ids = frozenset().union(*adapter.expected_ids_by_source.values())
        if not validate_proof(ready, scope, exact_ids):
            fail("Readiness unproven")
        stage = "retrieval"
        operation = {}
        search = operation_search(row, adapter.settings, operation, scope, query)
        if capture:
            search["capture_context"] = True
        found = adapter.request("/search", search)
        ids = id_list(found.get("retrieved_ids"))
        if len(ids) > adapter.settings["top_k"]:
            fail("Retrieved IDs exceed top_k")
        check_scope_e1(ids, set(expected))
        evidence = trace_projection(found)
        for traced in evidence["stages"]:
            check_scope_e1(traced["chunk_ids"], set(expected))
        if evidence["coverage"] != "unavailable" and evidence["stages"][-1]["chunk_ids"] != ids:
            fail("Response evidence does not match retrieval order")
        check_scope_e1([item["chunk_id"] for item in found.get("rejections", [])], set(expected))
        result = trace_row_v2(found, row, manifest_hash)
        result.update(operation)
        result["ingested_ids"] = accepted
        if capture:
            artifact = context_artifact_v2(found, row, manifest_hash, ids)
            context_sink(artifact)
            result["context_sha256"] = canonical_hash(artifact)
        success = True
        return result
    except (OSError, ValueError, KeyError, TypeError, AttributeError):
        return failed_trace_row_v2(row, manifest_hash, stage)
    finally:
        adapter.close()
        if readiness_sink is not None and adapter.settings.get("capture_context", False):
            readiness_sink(build_artifact(adapter, row["question_id"], manifest_hash,
                                          stage, success, initialized))


def run_e1(args):
    """Create-only E1 run: manifest-bound, v2 sidecar, optional v2 contexts."""
    from runner import save_context_artifact
    manifest_hash = check_manifest(args.manifest, args.dataset, args.config)
    settings = settings_from_e1(args.config)
    contexts = getattr(args, "contexts", None)
    readiness_artifacts = getattr(args, "readiness_artifacts", None)
    if readiness_artifacts is not None and not settings.get("capture_context", False):
        fail("--readiness-artifacts requires frozen runner.capture_context=true")
    if (settings.get("capture_context", False) != (contexts is not None)
            and not (settings.get("capture_context", False) and readiness_artifacts is not None)):
        fail("--contexts requires frozen runner.capture_context=true and vice versa")
    rows = decode(args.dataset.read_text(), "dataset")
    for row in rows:
        question_payload_e1(row, settings)
    if args.dry_run:
        return {"mode": "dry_run", "arm": "E1", "expected": len(rows), "network_calls": 0,
                "manifest_sha256": manifest_hash}
    if args.trace.exists():
        fail("E1 runs are create-only; choose a new trace path")
    if contexts is not None:
        Path(contexts).mkdir(parents=True, exist_ok=True)
    if readiness_artifacts is not None:
        Path(readiness_artifacts).mkdir(parents=True, exist_ok=True)
    adapter = LocalEngineAdapterE1(settings)
    seen = {}
    with args.trace.open("x") as handle:
        for row in rows:
            sink = None if contexts is None else (
                lambda artifact: save_context_artifact(contexts, artifact))
            proof_sink = None if readiness_artifacts is None else (
                lambda artifact: save_artifact(readiness_artifacts, artifact))
            record = retrieve_e1(adapter, row, manifest_hash, context_sink=sink, readiness_sink=proof_sink)
            write_trace_row_v2(handle, record)
            seen[row["question_id"]] = record
    return {"mode": "retrieval_only", "arm": "E1", "expected": len(rows), "observed": len(seen),
            "failed": sum(record["status"] == "failed" for record in seen.values()),
            "manifest_sha256": manifest_hash, "accuracy": None}


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    for name in ("dataset", "config", "manifest", "trace"):
        parser.add_argument("--" + name, required=True, type=Path)
    parser.add_argument("--contexts", type=Path, help="Directory for actual returned v2 evidence")
    parser.add_argument("--readiness-artifacts", type=Path,
                        help="Directory for bounded historical readiness diagnostics (not a live lease)")
    parser.add_argument("--dry-run", action="store_true")
    args = parser.parse_args(argv)
    try:
        summary = run_e1(args)
    except (OSError, ValueError, KeyError, TypeError) as exc:
        print("Runner rejected artifacts: " + type(exc).__name__, file=sys.stderr)
        return 2
    print(json.dumps(summary, indent=2))
    return 2 if summary.get("failed") else 0


if __name__ == "__main__":
    sys.exit(main())
