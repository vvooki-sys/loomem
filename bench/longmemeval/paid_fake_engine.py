"""In-memory stand-in for ``paid_engine.EvaluationEngine`` (tests and binary-less dry-runs only).

It never launches a process. ``ingest_session`` sends ONE extraction request
through the provider proxy in the frozen extraction profile shape (native
keys, two messages, a system prompt with the per-session date line), so the
proxy, the journal, the ledger and the fake provider are exercised exactly as
with the real engine; the accepted facts come from the provider's answer.
``search`` returns one level-1 hit per fact and one level-0 fragment hit per
stored source whose content is the exact byte slice of that source, in the
shape ``paid_context`` validates. State is kept on disk under the work dir so
a later ``start(work, reuse=True)`` (retrieval) sees what extraction stored.
The session is one chunk: facts above ``max_facts_per_chunk`` are discarded as
the product does, and a real-shaped extraction receipt (``partial`` when facts
were discarded or an attempt failed) passes ``confirmed_facts_with_cap``; a
rejected receipt is an ``EngineError`` (a stop), as in ``paid_engine``.
Nothing here reads a gold field.
"""
import hashlib
import json
from pathlib import Path
import uuid
from urllib.error import HTTPError
from urllib.request import ProxyHandler, Request, build_opener

from extraction_receipt import confirmed_facts_with_cap
from ingest_receipt import ReceiptError
from paid_engine import EngineError, SessionReceipt, render_transcript

SYSTEM_TEXT = ("FAKE ENGINE extraction system prompt (not the frozen one).\n"
               "The conversation_date is: {date}")
NAMESPACE = uuid.UUID("3f2a9c1e-6d7b-4e0f-9a21-5c4b8d7e6f10")


class FakeEngineError(RuntimeError):
    pass


def _uuid(*parts):
    return str(uuid.uuid5(NAMESPACE, "|".join(parts)))


def _expected(fact, source_id, model):
    """The engine's pre-persistence projection of a freshly generated fact (``extraction_receipt._projection``)."""
    meta = {"fact_type": "fact", "subject": None, "event_date": None, "event_date_context": None, "supersedes": None,
            "superseded_by": None, "confidence": 0.9, "extracted_from": source_id, "extraction_model": model,
            "original_content": None, "topic": None, "attributed_to": None}
    return {"content": fact["content"], "level": 1, "score": 1.0, "timestamp": 0, "updated_at": 0, "consolidated": False,
            "source_ids": [source_id], "metadata": {"_loomem_ingest": {"version": 1, "kind": "fact",
                                                                       "conversation_date": fact["conversation_date"]}},
            "extraction_meta": meta, "source": {"agent": "mcp-ingest"}, "trust_level": "a2", "provenance_role": "Claim",
            "persistent": True, "importance": 0.9, "memory_type": "static", "valid_from": None, "valid_until": None,
            "is_latest": True, "superseded_by": None, "supersedes_id": None, "dormant": False, "in_progress": False,
            "deleted_at": None, "fragment": None}


class FakeEngine:
    def __init__(self, proxy_url, proxy_token, *, profile=None, top_k=60, max_facts_per_chunk=None):
        self.proxy_url, self.proxy_token, self.top_k = proxy_url, proxy_token, top_k
        self.max_facts_per_chunk = max_facts_per_chunk  # None: no cap (tests that build the engine directly)
        self.profile = profile or {"model": "gpt-6-luna", "reasoning_effort": "low", "max_completion_tokens": 16384}
        self.effective_config_sha256 = hashlib.sha256(b"fake-engine-effective-config").hexdigest()
        self.work = None
        self.state = {"sources": {}, "facts": {}}
        self.opener = build_opener(ProxyHandler({}))
        self.requests = []

    # -- lifecycle -------------------------------------------------------------
    def start(self, work, reuse=False):
        self.work = Path(work)
        state = self.work / "data" / "fake_engine_state.json"
        if reuse:
            if not state.is_file():
                raise FakeEngineError(f"nothing to reuse in {self.work}")
            self.state = json.loads(state.read_text(encoding="utf-8"))
            return
        self.work.mkdir(parents=True, exist_ok=True)
        if any(self.work.iterdir()):
            raise FakeEngineError(f"work directory is not empty: {self.work}")
        (self.work / "data").mkdir()
        (self.work / "config.toml").write_text("# fake engine\n")
        self._save()

    def _save(self):
        (self.work / "data" / "fake_engine_state.json").write_text(json.dumps(self.state, sort_keys=True), encoding="utf-8")

    def close(self):
        pass

    def readiness_probe(self):
        return 400  # the fake behaves like an evaluation build

    def fingerprints(self):
        return {"binary_sha256": "fake", "template_sha256": "fake", "model_onnx_sha256": "fake",
                "tokenizer_sha256": "fake", "effective_config_sha256": self.effective_config_sha256,
                "sandbox_profile_sha256": "fake"}

    # -- the one provider call per session (through the proxy) ------------------
    def _extract(self, content, conversation_date):
        body = {"model": self.profile["model"],
                "messages": [{"role": "system", "content": SYSTEM_TEXT.format(date=conversation_date or "unknown")},
                             {"role": "user", "content": content}],
                "max_completion_tokens": self.profile["max_completion_tokens"],
                "reasoning_effort": self.profile["reasoning_effort"]}
        request = Request(self.proxy_url.rstrip("/") + "/chat/completions", data=json.dumps(body).encode("utf-8"),
                          method="POST", headers={"Content-Type": "application/json",
                                                  "Authorization": "Bearer " + self.proxy_token})
        try:
            with self.opener.open(request, timeout=30) as response:
                status, raw = response.status, response.read()
        except HTTPError as error:
            with error:
                status, raw = error.code, error.read()
        self.requests.append({"status": status, "bytes": len(raw)})
        if status != 200:
            raise FakeEngineError(f"extraction request failed: HTTP {status} {raw[:200]!r}")
        answer = json.loads(raw.decode("utf-8"))
        try:
            parsed = json.loads(answer["choices"][0]["message"]["content"])
            facts = parsed.get("facts") or []
        except (KeyError, IndexError, TypeError, ValueError):
            parsed, facts = {}, []
        # `_fake_outcome`: a fake-provider-only override of the receipt outcome (e.g. a failed attempt).
        override = parsed.get("_fake_outcome") or {}
        return [f for f in facts if isinstance(f, dict) and isinstance(f.get("content"), str)], override

    def _confirm(self, source_id, facts, generated, override):
        """Build the engine's extraction receipt for one chunk and validate it exactly as ``paid_engine`` does."""
        limit = self.max_facts_per_chunk
        outcome = {"input_chunks_total": 1, "input_chunks_processed": 1, "calls_attempted": 1, "calls_completed": 1,
                   "calls_truncated": 0, "failed_attempts": 0,
                   "fact_limit_reached": limit is not None and generated >= limit,
                   "discarded_facts": generated - len(facts), "accepted_facts": len(facts), **override}
        receipt = {"version": 1, "scope": "generated_facts", "evidence": "pre_persistence_expected_records_v1",
                   "source_id": source_id, "error_code": None,
                   "status": "partial" if outcome["discarded_facts"] or outcome["failed_attempts"] else "complete",
                   "outcome": outcome, "persistence": {"stored": len(facts), "failed": 0, "identity_mismatches": 0,
                                                       "invalid_expectations": 0},
                   "facts": [{"accepted_ordinal": f["accepted_ordinal"], "id": f["id"], "allocated_id": f["id"],
                              "expected": _expected(f, source_id, self.profile["model"])} for f in facts]}
        try:
            return confirmed_facts_with_cap(receipt, source_id)[1]
        except ReceiptError as exc:
            raise EngineError(f"memory_ingest receipt rejected: {exc} (fake engine)") from exc

    def ingest_session(self, session):
        content = render_transcript(session)
        sid = session["session_id"]
        source_id = _uuid("source", sid)
        if source_id in self.state["sources"]:
            raise FakeEngineError(f"duplicate source {source_id}")
        raw_facts, override = self._extract(content, session.get("conversation_date"))
        kept = raw_facts if self.max_facts_per_chunk is None else raw_facts[:self.max_facts_per_chunk]
        facts = []
        for ordinal, fact in enumerate(kept):
            fid = _uuid("fact", sid, str(ordinal))
            facts.append({"accepted_ordinal": ordinal, "id": fid, "allocated_id": fid, "expected": True,
                          "content": fact["content"], "source_id": source_id,
                          "conversation_date": session.get("conversation_date")})
        cap = self._confirm(source_id, facts, len(raw_facts), override)
        self.state["sources"][source_id] = {"session_id": sid, "content": content,
                                            "conversation_date": session.get("conversation_date")}
        self.state["facts"][source_id] = facts
        self._save()
        return SessionReceipt(sid, source_id, content, session.get("conversation_date"), [f"{source_id}:f0"],
                              tuple(facts), [f["id"] for f in facts], len(facts) == 0, {"fake": True},
                              cap.capped, cap.discarded_facts)

    def prove_readiness_v2(self, scope, receipt, session, deadline_s):
        ids = [receipt.source_id, *receipt.fragment_ids, *receipt.fact_ids]
        return {"request": {"schema_version": 1, "scope": scope, "source": {"source_id": receipt.source_id}},
                "response": {"schema_version": 2, "scope": scope, "ready": True,
                             "records": [{"id": i, "chunk": "active", "vector": "valid", "document": "current"} for i in ids]}}

    def fetch_source(self, source_id):
        source = self.state["sources"].get(source_id)
        if source is None:
            raise FakeEngineError(f"unknown source {source_id}")
        return {"id": source_id, "content": source["content"]}

    def search(self, question, question_date, top_k=None):
        results = []
        for source_id, facts in sorted(self.state["facts"].items()):
            source = self.state["sources"][source_id]
            for fact in facts:
                results.append({"id": fact["id"], "content": fact["content"], "score": 0.9, "metadata": {"level": 1},
                                "evidence": {"author": None, "conversation_date": source["conversation_date"],
                                             "event_date": None, "event_date_provenance": None,
                                             "source_id": source_id, "byte_start": None, "byte_end": None}})
        for source_id, source in sorted(self.state["sources"].items()):
            size = len(source["content"].encode("utf-8"))
            results.append({"id": f"{source_id}:f0", "content": source["content"], "score": 0.5,
                            "metadata": {"level": 0},
                            "evidence": {"author": None, "conversation_date": source["conversation_date"],
                                         "event_date": None, "event_date_provenance": None,
                                         "source_id": source_id, "byte_start": 0, "byte_end": size}})
        return {"results": results[: (top_k or self.top_k)], "trace_metadata": {"fake": True}}


def fake_engine_factory(campaign):
    """``factories["engine"]`` entry for ``paid_runner.main``: no binary, no sandbox, no process."""
    profile = campaign.profile("extraction")
    top_k = campaign.spec.get("search", {}).get("top_k", 60)
    limit = campaign.spec.get("extraction", {}).get("max_facts_per_chunk")
    return lambda proxy_url, proxy_token: FakeEngine(proxy_url, proxy_token, profile=profile, top_k=top_k,
                                                     max_facts_per_chunk=limit)
