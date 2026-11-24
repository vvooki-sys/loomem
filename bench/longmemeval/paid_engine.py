"""One owned, sandboxed Loomem engine per history for the paid extraction run.

Config = the frozen ``config.toml`` template through ``runner_local.
effective_config`` plus the owner-frozen overrides: knowledge extraction ON
(chunked, ``[llm] provider = "bench"`` pointing at the provider proxy, key env
``LOOMEM_BENCH_PROXY_TOKEN``), fragments ON, ``[search] evidence_fields``
ON, every other LLM mutator OFF, local 384-d embeddings, loopback host and a
random port. The process runs under ``sandbox-exec`` (loopback-only outbound)
with a minimal env that never carries ``OPENAI_API_KEY``. Every engine answer
is validated fail-closed: ``memory_ingest`` through the structured ingest and
extraction receipts, readiness through ``/v1/evaluation/ingest-readiness-v2``
plus the exact-bytes projection check, stored sources by re-reading them.
"""
from dataclasses import dataclass
from datetime import date
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import secrets
import socket
import subprocess
import time
from urllib.error import HTTPError
from urllib.parse import quote
from urllib.request import ProxyHandler, Request, build_opener

from extraction_receipt import confirmed_facts_with_cap
from fragment_projection import EVIDENCE_V2, v2_projection_proven
from ingest_receipt import ReceiptError, expected_ids_from_mcp_result
from paid_engine_config import (DISABLED_SECTIONS, EngineError, check_artifacts, check_extraction, check_port,  # noqa: F401
                                config_fingerprint, evaluation_config, free_port, sandbox_wrapper)
from paid_provider_proxy import KEY_ENV
from readiness_proof import STATES
from runner_local_e1 import render_transcript as _render_turns
from smoke_local_e1 import PROFILE, SANDBOX

STREAM = "__user_default__"
MAX_EXTRACTION_TIMEOUT_S = 600
READINESS_PATH = "/v1/evaluation/ingest-readiness-v2"
MAX_RECORDS, MAX_READINESS_BYTES = 1024, 2 * 1024 * 1024
MCP_CLIENT = {"name": "loomem-longmemeval-paid", "version": "1"}
START_DEADLINE_S, POLL_S, READY_POLL_S = 120, 0.05, 0.25
_LINES = re.compile(r"[^\n]*\n|[^\n]+")


@dataclass(frozen=True)
class SessionReceipt:
    session_id: str
    source_id: str
    content: str
    conversation_date: str | None
    fragment_ids: list
    facts: tuple
    fact_ids: list
    zero_facts: bool
    raw: dict
    capped: bool = False            # the per-chunk fact cap discarded facts (engine status `partial`)
    discarded_facts: int = 0


def _iso_date(value, label):
    try:
        if not isinstance(value, str) or date.fromisoformat(value).isoformat() != value:
            raise ValueError
    except ValueError as exc:
        raise EngineError(f"{label} must be YYYY-MM-DD, got {value!r}") from exc
    return value


def render_transcript(session):
    """`role: content` per turn (runner_local_e1), EngineError instead of ValueError."""
    try:
        return _render_turns(session.get("turns") if isinstance(session, dict) else None)
    except ValueError as exc:
        raise EngineError(f"session cannot be rendered: {exc}") from exc


def predicted_chunks(text, chunk_chars):
    """Chunk count of chunking.rs line_aligned_chunks: greedy line-aligned, char counts."""
    if type(chunk_chars) is not int or chunk_chars <= 0:
        raise ValueError("chunk_chars must be a positive int")
    chunks = current = 0
    for line in _LINES.findall(text):
        if current > 0 and current + len(line) > chunk_chars:
            chunks, current = chunks + 1, 0
        current += len(line)
    return chunks + 1 if current > 0 or chunks == 0 else chunks


def _check_proof(response, scope, expected_ids):
    """Shape and state check of one readiness-v2 answer; returns its readiness."""
    fields = {"schema_version", "scope", "evidence", "dimension", "ready", "records", "fragment_projections"}
    if not isinstance(response, dict) or set(response) != fields:
        raise EngineError("readiness-v2 proof has unexpected fields")
    if (response["schema_version"] != 2 or response["scope"] != scope or response["evidence"] != EVIDENCE_V2
            or response["dimension"] != 384 or type(response["ready"]) is not bool
            or not isinstance(response["records"], list)):
        raise EngineError("readiness-v2 proof identity mismatch")
    seen, complete = set(), True
    for record in response["records"]:
        if not isinstance(record, dict) or set(record) != {"id", *STATES} or record["id"] in seen:
            raise EngineError("readiness-v2 record malformed or duplicated")
        seen.add(record["id"])
        for key, states in STATES.items():
            if record[key] not in states:
                raise EngineError(f"readiness-v2 record state {record[key]!r} unknown for {key}")
        complete &= (record["chunk"], record["vector"], record["document"]) == ("active", "valid", "current")
    if seen != set(expected_ids) or response["ready"] != complete:
        raise EngineError("readiness-v2 records do not match the receipts exactly")
    if not complete and response["fragment_projections"] is not None:
        raise EngineError("readiness-v2 exposed projections while unready")
    return complete


class EvaluationEngine:
    def __init__(self, artifacts, *, proxy_url, proxy_token, extraction, top_k, port=None,
                 extraction_timeout_s=MAX_EXTRACTION_TIMEOUT_S):
        self.artifacts = check_artifacts(artifacts)
        self.extraction = check_extraction(extraction)
        if type(top_k) is not int or top_k <= 0:
            raise EngineError("top_k must be a positive int")
        # The evaluation build reads LOOMEM_EVALUATION_EXTRACTION_TIMEOUT_SECS (memory_extractor/http.rs);
        # without it the engine gives up on a paid completion after 30 s while the proxy still waits.
        if type(extraction_timeout_s) is not int or not 1 <= extraction_timeout_s <= MAX_EXTRACTION_TIMEOUT_S:
            raise EngineError(f"extraction_timeout_s must be an int in 1..{MAX_EXTRACTION_TIMEOUT_S}")
        self.extraction_timeout_s = extraction_timeout_s
        if not isinstance(proxy_token, str) or len(proxy_token) < 32:
            raise EngineError("proxy_token must be the proxy's random bearer")
        self.proxy_url, self.proxy_token, self.top_k = proxy_url, proxy_token, top_k
        self.port = free_port() if port is None else check_port(port)
        self.token = secrets.token_hex(32)
        self.endpoint = f"http://127.0.0.1:{self.port}"
        self.effective_config_sha256 = config_fingerprint(self.artifacts, self.extraction)
        self.config_sha256 = self.process = self.work = None
        self.sources, self.session_id, self.rpc_id = {}, None, 0
        self.opener = build_opener(ProxyHandler({}))

    # -- process -------------------------------------------------------------
    def engine_env(self, work):
        work = Path(work)
        return {"PATH": os.defpath, "HOME": str(work), "TMPDIR": str(work / "tmp"), "RUST_LOG": "error",
                "LOOMEM_CONFIG": str(work / "config.toml"), "LOOMEM_AUTH_TOKEN": self.token,
                "LOOMEM_EVALUATION_READINESS": "1", KEY_ENV: self.proxy_token,
                "LOOMEM_EVALUATION_EXTRACTION_TIMEOUT_SECS": str(self.extraction_timeout_s)}

    def start(self, work, reuse=False):
        if platform.system() != "Darwin" or not Path(SANDBOX).is_file():
            raise EngineError("the evaluation engine runs only under macOS sandbox-exec")
        work = Path(work).resolve()
        if reuse:
            if not (work / "config.toml").is_file() or not (work / "data").is_dir():
                raise EngineError(f"nothing to reuse in {work}")
        else:
            work.mkdir(parents=True, exist_ok=True)
            if any(work.iterdir()):
                raise EngineError(f"work directory is not empty: {work}")
            (work / "entities.toml").write_text("")
            (work / "synonyms.toml").write_text("synonyms = []\n")
            (work / "pii_blocklist.txt").write_text("")
        text = evaluation_config(self.artifacts, work, port=self.port, proxy_url=self.proxy_url,
                                 extraction=self.extraction)
        (work / "config.toml").write_text(text)
        (work / "tmp").mkdir(exist_ok=True)
        self.config_sha256 = hashlib.sha256(text.encode("utf-8")).hexdigest()
        wrapper = sandbox_wrapper(work, Path(self.artifacts["binary"]))
        with socket.socket() as probe:  # fail before launching when the port is taken
            probe.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
            probe.bind(("127.0.0.1", self.port))
        self.work, self.session_id, self.rpc_id, self.sources = work, None, 0, {}
        with (work / "engine.log").open("ab") as log:
            self.process = subprocess.Popen([str(wrapper)], cwd=work, env=self.engine_env(work),
                                            stdout=subprocess.DEVNULL, stderr=log)
        try:
            self._wait_ready(reuse)
        except BaseException:  # a failed start never leaves a live child holding the database
            self.close()
            raise

    def _wait_ready(self, reuse):
        deadline = time.monotonic() + START_DEADLINE_S
        while time.monotonic() < deadline:
            if self.process.poll() is not None:
                raise EngineError(f"engine exited with {self.process.returncode}: {self._log_tail()}")
            try:
                summary = self._get("/v1/status")["config_summary"]
            except OSError:
                time.sleep(POLL_S)
                continue
            if not reuse and (summary["tantivy_docs"] != 0 or summary["embeddings_count"] != 0):
                raise EngineError("a fresh engine database must start empty")
            return
        raise EngineError(f"engine startup timeout: {self._log_tail()}")

    def _log_tail(self):
        try:
            return (self.work / "engine.log").read_bytes()[-1500:].decode("utf-8", "replace")
        except OSError:
            return ""

    def close(self):
        if self.process is not None:
            self.process.terminate()
            try:
                self.process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait(timeout=10)
            self.process = None

    def fingerprints(self):
        return {"binary_sha256": self.artifacts["binary_sha256"], "template_sha256": self.artifacts["template_sha256"],
                "model_onnx_sha256": self.artifacts["model_onnx_sha256"], "tokenizer_sha256": self.artifacts["tokenizer_sha256"],
                "effective_config_sha256": self.effective_config_sha256,
                "sandbox_profile_sha256": hashlib.sha256(PROFILE.encode("utf-8")).hexdigest()}

    # -- HTTP seam (override _post/_get/_mcp to test without a process) --------
    def _request(self, method, path, body=None, headers=None):
        if self.process is not None and self.process.poll() is not None:
            raise EngineError(f"engine exited with {self.process.returncode}: {self._log_tail()}")
        data = None if body is None else json.dumps(body).encode("utf-8")
        merged = {"Content-Type": "application/json", "Accept": "application/json",
                  "Authorization": "Bearer " + self.token, **(headers or {})}
        request = Request(self.endpoint + path, data=data, method=method, headers=merged)
        try:
            with self.opener.open(request, timeout=120) as response:
                return response.status, _lower(response.headers), response.read()
        except HTTPError as error:
            with error:
                return error.code, _lower(error.headers), error.read()

    @staticmethod
    def _json(status, raw, path):
        if status != 200:
            raise EngineError(f"{path}: HTTP {status}: {raw[:300]!r}")
        try:
            found = json.loads(raw.decode("utf-8"))
        except (UnicodeDecodeError, ValueError) as exc:
            raise EngineError(f"{path}: response is not JSON") from exc
        if not isinstance(found, dict):
            raise EngineError(f"{path}: response is not an object")
        return found

    def _post(self, path, body):
        status, _, raw = self._request("POST", path, body)
        return self._json(status, raw, path)

    def readiness_probe(self):
        """HTTP status of a deliberately bad readiness-v2 body: 400/422 on an evaluation build, 404 otherwise."""
        status, _, _ = self._request("POST", READINESS_PATH, {"schema_version": 1})
        return status

    def _get(self, path):
        status, _, raw = self._request("GET", path)
        return self._json(status, raw, path)

    def _mcp(self, body, session_id):
        headers = {} if session_id is None else {"Mcp-Session-Id": session_id}
        status, found_headers, raw = self._request("POST", "/mcp", body, headers)
        return self._json(status, raw, "/mcp"), found_headers.get("mcp-session-id")

    # -- MCP -------------------------------------------------------------------
    def rpc(self, method, params):
        self.rpc_id += 1
        found, session = self._mcp({"jsonrpc": "2.0", "id": self.rpc_id, "method": method, "params": params},
                                   self.session_id)
        if (not isinstance(found, dict) or found.get("jsonrpc") != "2.0" or found.get("id") != self.rpc_id
                or "error" in found or not isinstance(found.get("result"), dict)):
            raise EngineError(f"MCP {method}: not a JSON-RPC success for this request")
        return found["result"], session

    def tool_call(self, name, arguments):
        if self.session_id is None:
            result, session = self.rpc("initialize", {"protocolVersion": "2025-03-26", "capabilities": {},
                                                      "clientInfo": MCP_CLIENT})
            if not isinstance(session, str) or not session or "protocolVersion" not in result:
                raise EngineError("MCP initialize did not open a session")
            self.session_id = session
        return self.rpc("tools/call", {"name": name, "arguments": arguments})[0]

    # -- operations --------------------------------------------------------------
    def ingest_session(self, session):
        session_id = session.get("session_id") if isinstance(session, dict) else None
        if not isinstance(session_id, str) or not session_id:
            raise EngineError("session_id required")
        content = render_transcript(session)
        arguments = {"content": content}
        if session.get("conversation_date") is not None:
            arguments["conversation_date"] = _iso_date(session["conversation_date"], "conversation_date")
        result = self.tool_call("memory_ingest", arguments)
        if not isinstance(result, dict) or result.get("isError", False) is not False:
            raise EngineError(f"memory_ingest returned an error: {_result_text(result)}")
        try:
            expected = expected_ids_from_mcp_result(result)
            structured = result["structuredContent"]
            if "extraction_receipt" not in structured:
                raise EngineError("memory_ingest result lacks extraction_receipt")
            source_id = structured["ingest_receipt"]["source_id"]
            facts, cap = confirmed_facts_with_cap(structured["extraction_receipt"], source_id)
        except ReceiptError as exc:
            raise EngineError(f"memory_ingest receipt rejected: {exc} ({_result_text(result)})") from exc
        if source_id in self.sources:
            raise EngineError(f"duplicate source id {source_id}")
        self.sources[source_id] = content
        return SessionReceipt(session_id, source_id, content, arguments.get("conversation_date"),
                              sorted(expected - {source_id}), facts, [fact["id"] for fact in facts],
                              len(facts) == 0, result, cap.capped, cap.discarded_facts)

    def prove_readiness_v2(self, scope, receipt, session, deadline_s):
        if not isinstance(scope, str) or not scope or len(scope) > 128:
            raise EngineError("scope must be a nonempty string of at most 128 characters")
        facts = [{key: fact[key] for key in ("accepted_ordinal", "id", "allocated_id", "expected")} for fact in receipt.facts]
        request = {"schema_version": 1, "scope": scope, "stream": STREAM,
                   "source": {"source_id": receipt.source_id, "content": receipt.content,
                              "conversation_date": receipt.conversation_date, "fragment_ids": list(receipt.fragment_ids)},
                   "facts": facts}
        if len(receipt.fragment_ids) + len(facts) >= MAX_RECORDS or len(json.dumps(request).encode()) > MAX_READINESS_BYTES:
            raise EngineError("session exceeds the readiness-v2 request limits")
        expected = {receipt.source_id, *receipt.fragment_ids, *receipt.fact_ids}
        deadline = time.monotonic() + deadline_s
        while True:
            response = self._post(READINESS_PATH, request)
            if _check_proof(response, scope, expected):
                proof = {"request": request, "response": response}
                if not v2_projection_proven(proof, session):
                    raise EngineError("readiness-v2 projections do not match the exact session bytes")
                return proof
            if time.monotonic() >= deadline:
                raise EngineError(f"readiness-v2 not reached within {deadline_s} s")
            time.sleep(READY_POLL_S)

    def fetch_source(self, source_id):
        if source_id not in self.sources:
            raise EngineError(f"unknown source {source_id!r}")
        item = self._get("/api/memories/" + quote(source_id, safe=""))
        if item.get("id") != source_id:
            raise EngineError("stored source identity mismatch")
        if item.get("content") != self.sources[source_id]:
            raise EngineError("stored_source_changed")
        return item

    def search(self, question, question_date, top_k=None):
        if not isinstance(question, str) or not question.strip():
            raise EngineError("question must be nonempty text")
        top_k = self.top_k if top_k is None else top_k
        if type(top_k) is not int or top_k <= 0:
            raise EngineError("top_k must be a positive int")
        found = self._post("/v1/search", {"query": question, "top_k": top_k, "trace": True, "dry_run": True,
                                          "reference_time": _iso_date(question_date, "question_date") + "T00:00:00Z"})
        if not isinstance(found.get("results"), list):
            raise EngineError("/v1/search answered without results")
        return found


def _lower(headers):
    return {key.lower(): value for key, value in headers.items()}


def _result_text(result):
    try:
        return str(result["content"][0]["text"])[:300]
    except (KeyError, IndexError, TypeError):
        return ""
