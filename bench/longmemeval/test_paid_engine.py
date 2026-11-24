import copy
import hashlib
import json
import os
from pathlib import Path
import platform
import tempfile
import tomllib
import unittest
from unittest.mock import patch

import paid_engine
from paid_engine import (EngineError, EvaluationEngine, SessionReceipt, config_fingerprint, evaluation_config,
                         predicted_chunks, render_transcript, sandbox_wrapper)
from paid_engine_config import DISABLED_SECTIONS, FORBIDDEN_PORTS
from paid_provider_proxy import KEY_ENV
from paid_test_support import history_doc

TEMPLATE = Path(__file__).resolve().parents[2] / "config.toml"
EXTRACTION = {"max_completion_tokens": 16384, "input_chunk_tokens": 4000, "split_on_truncation": True,
              "max_facts_per_chunk": 20, "completion_tokens_floor": 0, "max_transcript_tokens": 20000}
SOURCE = "00000000-0000-4000-8000-000000000001"
FACT = "00000000-0000-4000-8000-000000000002"
PROXY_TOKEN = "p" * 64


def sha(data):
    return hashlib.sha256(data).hexdigest()


def fake_artifacts(root):
    root = Path(root)
    binary = root / "loomem-server"
    binary.write_bytes(b"#!/bin/sh\nexit 0\n")
    model = root / "model"
    model.mkdir()
    (model / "model.onnx").write_bytes(b"onnx")
    (model / "tokenizer.json").write_bytes(b"{}")
    return {"binary": str(binary), "binary_sha256": sha(binary.read_bytes()),
            "template": str(TEMPLATE), "template_sha256": sha(TEMPLATE.read_bytes()),
            "model_dir": str(model), "model_onnx_sha256": sha(b"onnx"), "tokenizer_sha256": sha(b"{}")}


def fact_row(conversation_date="2025-07-10"):
    expected = dict(content="A synthetic fact.", level=1, score=1.0, timestamp=100, updated_at=100, consolidated=False,
                    source_ids=[SOURCE], metadata={"_loomem_ingest": dict(version=1, kind="fact", conversation_date=conversation_date)},
                    extraction_meta=dict(fact_type="fact", subject=None, event_date=None, event_date_context=None,
                                         supersedes=None, superseded_by=None, confidence=0.9, extracted_from=SOURCE,
                                         extraction_model="synthetic", original_content=None, topic=None, attributed_to=None),
                    source={"agent": "mcp-ingest"}, trust_level="a2", provenance_role="Claim", persistent=True,
                    importance=0.9, memory_type="static", valid_from=None, valid_until=None, is_latest=True,
                    superseded_by=None, supersedes_id=None, dormant=False, in_progress=False, deleted_at=None, fragment=None)
    return dict(accepted_ordinal=0, id=FACT, allocated_id=FACT, expected=expected)


def mcp_result(facts=1, fragments=2, extraction=True, is_error=False):
    ids = [f"{SOURCE}:f{i}" for i in range(fragments)]
    structured = {"ingest_receipt": dict(version=1, scope="source_and_fragments", status="complete", source_id=SOURCE,
                                         fragment_ids=ids, fragments=dict(stored=fragments, failed=0, skipped=0,
                                                                          segmentation_status="complete"))}
    if extraction:
        rows = [fact_row()] if facts else []
        structured["extraction_receipt"] = dict(
            version=1, scope="generated_facts", evidence="pre_persistence_expected_records_v1", source_id=SOURCE,
            status="complete", error_code=None,
            outcome=dict(input_chunks_total=1, input_chunks_processed=1, calls_attempted=1, calls_completed=1,
                         calls_truncated=0, failed_attempts=0, fact_limit_reached=False, discarded_facts=0, accepted_facts=facts),
            persistence=dict(stored=facts, failed=0, identity_mismatches=0, invalid_expectations=0), facts=rows)
    text = f"Source stored (id: {SOURCE}). Extracted {facts} facts from conversation, stored {facts}, skipped 0. Stored {fragments} fragments."
    return {"content": [{"type": "text", "text": text}], "isError": is_error, "structuredContent": structured}


class FakeMcp:
    def __init__(self, results):
        self.results, self.calls = list(results), []

    def __call__(self, body, session_id):
        self.calls.append((copy.deepcopy(body), session_id))
        if body["method"] == "initialize":
            return {"jsonrpc": "2.0", "id": body["id"], "result": {"protocolVersion": "2025-03-26"}}, "sess-1"
        if session_id != "sess-1":
            return {"jsonrpc": "2.0", "id": body["id"], "error": {"code": -32600, "message": "no session"}}, None
        return {"jsonrpc": "2.0", "id": body["id"], "result": self.results.pop(0)}, "sess-1"


class ConfigTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        self.artifacts = fake_artifacts(self.root)
        self.work = self.root / "engine" / "h1"

    def test_effective_config_carries_the_frozen_values(self):
        text = evaluation_config(self.artifacts, self.work, port=40123, proxy_url="http://127.0.0.1:40124/v1",
                                 extraction=EXTRACTION)
        config = tomllib.loads(text)
        for section in DISABLED_SECTIONS:
            node = config
            for part in section.split("."):
                node = node[part]
            self.assertIs(node["enabled"], False, section)
        llm = config["llm"]
        self.assertEqual((llm["provider"], llm["compression_model"]), ("bench", "gpt-6-luna"))
        self.assertEqual((llm["embedding_provider"], llm["embedding_dim"]), ("local", 384))
        self.assertEqual((llm["api_key"], llm["api_key_env"]), ("", "BENCH_NO_API_KEY"))
        self.assertEqual(llm["embedding_model_path"], self.artifacts["model_dir"])
        bench = llm["providers"]["bench"]
        self.assertEqual(bench, {"base_url": "http://127.0.0.1:40124/v1", "api_key_env": KEY_ENV, "omit_temperature": True,
                                 "max_completion_tokens": True, "completion_tokens_floor": 0, "reasoning_effort": "low"})
        ke = config["knowledge_extraction"]
        self.assertIs(ke["enabled"], True)
        self.assertEqual((ke["max_tokens"], ke["input_chunk_tokens"], ke["split_on_truncation"], ke["max_facts_per_chunk"]),
                         (16384, 4000, True, 20))
        self.assertEqual(ke["max_transcript_tokens"], 20000)
        self.assertIs(ke["contradiction_check"], False)
        self.assertEqual(config["fragments"], {**config["fragments"], "enabled": True, "provenance_role": "claim",
                                               "token_counter": "embedder"})
        self.assertIs(config["search"]["evidence_fields"], True)
        self.assertIs(config["search"]["rerank_enabled"], False)
        self.assertIs(config["search"]["multi_query_enabled"], False)
        from paid_engine_config import W0_FLAGS
        for section, key, want in W0_FLAGS:  # the nine W0 levers, explicitly off/neutral
            self.assertEqual(config[section][key], want, f"{section}.{key}")
            self.assertIs(type(config[section][key]), type(want), f"{section}.{key}")
        self.assertEqual((config["server"]["host"], config["server"]["port"]), ("127.0.0.1", 40123))
        self.assertNotIn(config["server"]["port"], FORBIDDEN_PORTS)
        self.assertEqual(config["server"]["auth_token_env"], "LOOMEM_AUTH_TOKEN")
        self.assertEqual(config["storage"]["data_dir"], str(self.work / "data"))
        self.assertIs(config["storage"]["vector_enabled"], True)
        self.assertIs(config["storage"]["tantivy"]["enabled"], True)
        self.assertNotIn("OPENAI_API_KEY", json.dumps({k: v for k, v in llm.items() if k != "providers"}))

    def test_fingerprint_is_deterministic_and_binding_independent(self):
        first = config_fingerprint(self.artifacts, EXTRACTION)
        self.assertEqual(first, config_fingerprint(self.artifacts, dict(EXTRACTION)))
        self.assertEqual(len(first), 64)
        self.assertNotEqual(first, config_fingerprint(self.artifacts, {**EXTRACTION, "max_transcript_tokens": 30000}))
        a = evaluation_config(self.artifacts, self.work, port=40123, proxy_url="http://127.0.0.1:1/v1", extraction=EXTRACTION)
        b = evaluation_config(self.artifacts, self.work, port=40125, proxy_url="http://127.0.0.1:2/v1", extraction=EXTRACTION)
        self.assertNotEqual(a, b)
        engine = EvaluationEngine(self.artifacts, proxy_url="http://127.0.0.1:1/v1", proxy_token=PROXY_TOKEN,
                                  extraction=EXTRACTION, top_k=60, port=40123)
        self.assertEqual(engine.effective_config_sha256, first)
        self.assertEqual(engine.fingerprints()["binary_sha256"], self.artifacts["binary_sha256"])
        self.assertEqual(set(engine.fingerprints()), {"binary_sha256", "template_sha256", "model_onnx_sha256",
                                                      "tokenizer_sha256", "effective_config_sha256", "sandbox_profile_sha256"})

    def test_refusals(self):
        good = dict(port=40123, proxy_url="http://127.0.0.1:1/v1", extraction=EXTRACTION)
        for port in sorted(FORBIDDEN_PORTS) + [80, 70000, "40123"]:
            with self.subTest(port=port), self.assertRaises(EngineError):
                evaluation_config(self.artifacts, self.work, **{**good, "port": port})
        for url in ("https://api.openai.com/v1", "http://localhost:5/v1", "http://127.0.0.1:5", None):
            with self.subTest(url=url), self.assertRaises(EngineError):
                evaluation_config(self.artifacts, self.work, **{**good, "proxy_url": url})
        with self.assertRaises(EngineError):
            evaluation_config(self.artifacts, Path("relative/work"), **good)
        for bad in ({**EXTRACTION, "max_tokens": 1}, {**EXTRACTION, "max_transcript_tokens": 0},
                    {**EXTRACTION, "split_on_truncation": 1}, {k: v for k, v in EXTRACTION.items() if k != "completion_tokens_floor"}):
            with self.subTest(bad=bad), self.assertRaises(EngineError):
                evaluation_config(self.artifacts, self.work, **{**good, "extraction": bad})
        with self.assertRaises(EngineError):
            evaluation_config({**self.artifacts, "binary_sha256": "0" * 64}, self.work, **good)
        with self.assertRaises(EngineError):
            evaluation_config({**self.artifacts, "model_dir": str(self.root / "missing")}, self.work, **good)
        for kwargs in (dict(top_k=0), dict(proxy_token="short"), dict(port=3035)):
            with self.subTest(kwargs=kwargs), self.assertRaises(EngineError):
                EvaluationEngine(self.artifacts, **{**dict(proxy_url="http://127.0.0.1:1/v1", proxy_token=PROXY_TOKEN,
                                                           extraction=EXTRACTION, top_k=60, port=40123), **kwargs})

    def test_extraction_timeout_reaches_the_engine_env_and_is_bounded(self):
        engine = EvaluationEngine(self.artifacts, proxy_url="http://127.0.0.1:1/v1", proxy_token=PROXY_TOKEN,
                                  extraction=EXTRACTION, top_k=60, port=40123, extraction_timeout_s=300)
        self.assertEqual(engine.engine_env(self.work)["LOOMEM_EVALUATION_EXTRACTION_TIMEOUT_SECS"], "300")
        default = EvaluationEngine(self.artifacts, proxy_url="http://127.0.0.1:1/v1", proxy_token=PROXY_TOKEN,
                                   extraction=EXTRACTION, top_k=60, port=40123)
        self.assertEqual(default.engine_env(self.work)["LOOMEM_EVALUATION_EXTRACTION_TIMEOUT_SECS"], "600")
        for bad in (0, 601, 5.0, "60"):
            with self.assertRaises(EngineError):
                EvaluationEngine(self.artifacts, proxy_url="http://127.0.0.1:1/v1", proxy_token=PROXY_TOKEN,
                                 extraction=EXTRACTION, top_k=60, port=40123, extraction_timeout_s=bad)

    @unittest.skipUnless(platform.system() == "Darwin" and Path("/usr/bin/sandbox-exec").is_file(),
                         "needs macOS sandbox-exec to launch the fake binary")
    def test_failed_start_closes_the_child(self):
        binary = self.root / "never-ready"
        binary.write_text("#!/bin/sh\nsleep 30\n")
        binary.chmod(0o700)
        artifacts = {**self.artifacts, "binary": str(binary), "binary_sha256": sha(binary.read_bytes())}
        engine = EvaluationEngine(artifacts, proxy_url="http://127.0.0.1:1/v1", proxy_token=PROXY_TOKEN,
                                  extraction=EXTRACTION, top_k=60)
        with patch("paid_engine.START_DEADLINE_S", 0.3):
            with self.assertRaisesRegex(EngineError, "startup timeout"):
                engine.start(self.root / "work-never-ready")
        self.assertIsNone(engine.process, "a failed start terminates and forgets the child")

    def test_engine_env_never_carries_a_provider_key(self):
        engine = EvaluationEngine(self.artifacts, proxy_url="http://127.0.0.1:1/v1", proxy_token=PROXY_TOKEN,
                                  extraction=EXTRACTION, top_k=60, port=40123)
        secrets_env = {"OPENAI_API_KEY": "sk-live-synthetic", "PAID_TEST_KEY": "paid-synthetic",
                       "LOOMEM_AUTH_TOKEN": "operator-token", "LOOMEM_BENCH_PROXY_TOKEN": "stale-proxy-token",
                       "HTTPS_PROXY": "http://proxy.invalid:3128"}
        with patch.dict(os.environ, secrets_env):
            env = engine.engine_env(self.work)
        self.assertNotIn("OPENAI_API_KEY", env)
        self.assertNotIn("HTTPS_PROXY", env)
        leaked = {k: v for k, v in env.items() if v in secrets_env.values()}
        self.assertEqual(leaked, {})
        self.assertEqual(env[KEY_ENV], PROXY_TOKEN)
        self.assertEqual(env["LOOMEM_AUTH_TOKEN"], engine.token)
        self.assertEqual((env["LOOMEM_EVALUATION_READINESS"], env["RUST_LOG"], env["PATH"]), ("1", "error", os.defpath))
        self.assertEqual(env["LOOMEM_CONFIG"], str(self.work / "config.toml"))
        self.assertEqual(set(env), {"PATH", "HOME", "TMPDIR", "RUST_LOG", "LOOMEM_CONFIG", "LOOMEM_AUTH_TOKEN",
                                    "LOOMEM_EVALUATION_READINESS", KEY_ENV, "LOOMEM_EVALUATION_EXTRACTION_TIMEOUT_SECS"})

    def test_sandbox_wrapper(self):
        wrapper = sandbox_wrapper(self.root, Path(self.artifacts["binary"]))
        text = wrapper.read_text()
        self.assertTrue(text.startswith("#!/bin/sh\nexec /usr/bin/sandbox-exec -p "))
        self.assertIn("deny network-outbound", text)
        self.assertTrue(text.rstrip().endswith(self.artifacts["binary"]))
        self.assertEqual(wrapper.stat().st_mode & 0o777, 0o700)


class ChunkTests(unittest.TestCase):
    def test_predicted_chunks_matches_line_aligned_chunks(self):
        self.assertEqual(predicted_chunks("", 10), 1)
        self.assertEqual(predicted_chunks("x" * 50, 10), 1, "a single long line is one chunk")
        self.assertEqual(predicted_chunks("x" * 50 + "\n", 10), 1)
        self.assertEqual(predicted_chunks("aaaa\nbbbb\ncccc", 10), 2)  # 5 + 5 fit, the third line starts chunk 2
        self.assertEqual(predicted_chunks("aaaa\nbbbb\ncccc", 9), 2)  # 5, then 5 + 4 = 9 still fits
        self.assertEqual(predicted_chunks("aaaa\nbbbb\ncccc", 8), 3)
        self.assertEqual(predicted_chunks("aaaa\nbbbb\ncccc\n", 10), 2)
        self.assertEqual(predicted_chunks("a\n" * 10, 4), 5)
        self.assertEqual(predicted_chunks("żółć\nżółć", 5), 2, "characters, not bytes")
        self.assertEqual(predicted_chunks("x" * 30 + "\n" + "y" * 5 + "\n" + "z" * 5, 10), 3)  # 31 | 6 | 5
        self.assertEqual(predicted_chunks("x" * 30 + "\n" + "y" * 4 + "\n" + "z" * 5, 10), 2)  # 31 | 5 + 5
        self.assertEqual(predicted_chunks("ab\rcd\x0bef\n", 3), 1, "only newline splits lines")
        with self.assertRaises(ValueError):
            predicted_chunks("x", 0)

    def test_render_transcript(self):
        session = history_doc("h1")["sessions"][0]
        self.assertEqual(render_transcript(session), "user: Synthetic turn 0 of h1.\nassistant: Noted.")
        with self.assertRaises(EngineError):
            render_transcript({"turns": [{"role": "system", "content": "x"}]})
        with self.assertRaises(EngineError):
            render_transcript({"turns": []})


class EngineOperationTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.artifacts = fake_artifacts(self.directory.name)
        self.engine = EvaluationEngine(self.artifacts, proxy_url="http://127.0.0.1:1/v1", proxy_token=PROXY_TOKEN,
                                       extraction=EXTRACTION, top_k=60, port=40123)
        self.session = history_doc("h1")["sessions"][0]
        self.content = render_transcript(self.session)

    def ingest(self, *results):
        mcp = FakeMcp(results)
        self.engine._mcp = mcp
        return mcp

    def test_ingest_session_zero_facts_is_a_state_not_an_error(self):
        mcp = self.ingest(mcp_result(facts=0))
        receipt = self.engine.ingest_session(self.session)
        self.assertIsInstance(receipt, SessionReceipt)
        self.assertTrue(receipt.zero_facts)
        self.assertEqual((receipt.facts, receipt.fact_ids), ((), []))
        self.assertEqual(receipt.fragment_ids, [f"{SOURCE}:f0", f"{SOURCE}:f1"])
        self.assertEqual((receipt.session_id, receipt.source_id, receipt.content, receipt.conversation_date),
                         ("h1-s1", SOURCE, self.content, "2025-07-10"))
        self.assertEqual([b["method"] for b, _ in mcp.calls], ["initialize", "tools/call"])
        self.assertEqual([s for _, s in mcp.calls], [None, "sess-1"])
        call = mcp.calls[1][0]["params"]
        self.assertEqual(call, {"name": "memory_ingest", "arguments": {"content": self.content, "conversation_date": "2025-07-10"}})
        self.assertEqual(self.engine.sources, {SOURCE: self.content})

    def test_ingest_session_with_a_fact_and_refusals(self):
        self.ingest(mcp_result(facts=1))
        receipt = self.engine.ingest_session(self.session)
        self.assertFalse(receipt.zero_facts)
        self.assertEqual(receipt.fact_ids, [FACT])
        self.assertEqual(receipt.facts[0]["expected"]["content"], "A synthetic fact.")
        self.engine.sources.clear()
        partial = mcp_result()
        partial["structuredContent"]["ingest_receipt"]["status"] = "partial"
        incomplete = mcp_result()
        incomplete["structuredContent"]["extraction_receipt"]["status"] = "partial"
        error = mcp_result(is_error=True)
        error["content"][0]["text"] = f"Source stored (id: {SOURCE}). Extraction failed (502): halted. No facts stored."
        cases = {"isError": error, "missing extraction_receipt": mcp_result(extraction=False), "partial ingest": partial,
                 "partial extraction": incomplete, "no structured": {"content": [{"type": "text", "text": "x"}]}}
        for label, result in cases.items():
            with self.subTest(case=label):
                self.ingest(result)
                self.engine.session_id, self.engine.sources = None, {}
                with self.assertRaises(EngineError):
                    self.engine.ingest_session(self.session)
        self.ingest(mcp_result(), mcp_result())
        self.engine.session_id, self.engine.sources = None, {}
        self.engine.ingest_session(self.session)
        with self.assertRaises(EngineError):
            self.engine.ingest_session({**self.session, "session_id": "h1-s2"})  # same source id twice
        with self.assertRaises(EngineError):
            self.engine.ingest_session({**self.session, "conversation_date": "2025/07/10"})
        undated = {**self.session, "conversation_date": None}
        mcp = self.ingest(mcp_result())
        self.engine.session_id, self.engine.sources = None, {}
        self.assertIsNone(self.engine.ingest_session(undated).conversation_date)
        self.assertNotIn("conversation_date", mcp.calls[1][0]["params"]["arguments"])

    def test_ingest_session_fact_limit_cap_is_a_state_not_an_error(self):
        capped = mcp_result()
        extraction = capped["structuredContent"]["extraction_receipt"]
        extraction["status"] = "partial"
        extraction["outcome"].update(fact_limit_reached=True, discarded_facts=2)
        self.ingest(capped)
        receipt = self.engine.ingest_session(self.session)
        self.assertEqual((receipt.capped, receipt.discarded_facts, receipt.zero_facts, receipt.fact_ids),
                         (True, 2, False, [FACT]))
        self.ingest(mcp_result())
        self.engine.session_id, self.engine.sources = None, {}
        receipt = self.engine.ingest_session(self.session)
        self.assertEqual((receipt.capped, receipt.discarded_facts), (False, 0))
        failed = copy.deepcopy(capped)
        failed["structuredContent"]["extraction_receipt"]["outcome"]["failed_attempts"] = 1
        self.ingest(failed)
        self.engine.session_id, self.engine.sources = None, {}
        with self.assertRaises(EngineError):
            self.engine.ingest_session(self.session)

    def receipt(self, facts=1):
        self.ingest(mcp_result(facts=facts))
        return self.engine.ingest_session(self.session)

    def proof_response(self, receipt, ready=True):
        encoded = self.content.encode()
        first = len("user: Synthetic turn 0 of h1.\n".encode())
        slices = [(f"{SOURCE}:f0", 0, first - 1, "user", 0), (f"{SOURCE}:f1", first, len(encoded), "assistant", 1)]
        projections = [{"id": ident, "content": encoded[start:end].decode(),
                        "fragment": {"byte_start": start, "byte_end": end, "conversation_date": "2025-07-10",
                                     "role": role, "source_id": SOURCE, "turn_index": index}}
                       for ident, start, end, role, index in slices]
        ids = [SOURCE, *receipt.fragment_ids, *receipt.fact_ids]
        state = ("active", "valid", "current") if ready else ("active", "missing", "current")
        records = [{"id": i, "chunk": state[0], "vector": state[1], "document": state[2]} for i in ids]
        return {"schema_version": 2, "scope": "scope-1", "evidence": "fresh_immutable_ingest_projections_v2",
                "dimension": 384, "ready": ready, "records": records, "fragment_projections": projections if ready else None}

    def test_prove_readiness_v2_accepts_exact_projections_and_polls(self):
        receipt = self.receipt()
        posted, answers = [], [self.proof_response(receipt, ready=False), self.proof_response(receipt)]
        self.engine._post = lambda path, body: (posted.append((path, body)), answers.pop(0))[1]
        with patch.object(paid_engine, "READY_POLL_S", 0.001):
            proof = self.engine.prove_readiness_v2("scope-1", receipt, self.session, 5)
        self.assertEqual([p for p, _ in posted], ["/v1/evaluation/ingest-readiness-v2"] * 2)
        request = posted[0][1]
        self.assertEqual(request["schema_version"], 1)
        self.assertEqual((request["scope"], request["stream"]), ("scope-1", "__user_default__"))
        self.assertEqual(request["source"], {"source_id": SOURCE, "content": self.content, "conversation_date": "2025-07-10",
                                             "fragment_ids": receipt.fragment_ids})
        self.assertEqual([set(f) for f in request["facts"]], [{"accepted_ordinal", "id", "allocated_id", "expected"}])
        self.assertEqual(proof, {"request": request, "response": self.proof_response(receipt)})

    def test_prove_readiness_v2_refusals(self):
        receipt = self.receipt()
        wrong_slice = self.proof_response(receipt)
        wrong_slice["response" if False else "fragment_projections"][0]["fragment"]["byte_end"] += 1
        missing_fact = self.proof_response(receipt)
        missing_fact["records"].pop()
        leaking = self.proof_response(receipt, ready=False)
        leaking["fragment_projections"] = []
        lying = self.proof_response(receipt, ready=False)
        lying["ready"] = True
        unknown_state = self.proof_response(receipt)
        unknown_state["records"][0]["chunk"] = "ok"
        v1 = {k: v for k, v in self.proof_response(receipt).items() if k != "fragment_projections"}
        for label, answer in {"wrong slice": wrong_slice, "missing fact record": missing_fact, "projection while unready": leaking,
                              "ready flag disagrees": lying, "unknown state": unknown_state, "v1 shape": v1}.items():
            with self.subTest(case=label):
                self.engine._post = lambda path, body, answer=answer: copy.deepcopy(answer)
                with self.assertRaises(EngineError):
                    self.engine.prove_readiness_v2("scope-1", receipt, self.session, 5)
        self.engine._post = lambda path, body: self.proof_response(receipt, ready=False)
        with self.assertRaises(EngineError):
            self.engine.prove_readiness_v2("scope-1", receipt, self.session, 0)
        with self.assertRaises(EngineError):
            self.engine.prove_readiness_v2("", receipt, self.session, 5)

    def test_fetch_source_and_search(self):
        receipt = self.receipt(facts=0)
        self.engine._get = lambda path: {"id": SOURCE, "content": self.content, "layer": "L0"}
        self.assertEqual(self.engine.fetch_source(SOURCE)["id"], SOURCE)
        self.engine._get = lambda path: {"id": SOURCE, "content": self.content + " "}
        with self.assertRaisesRegex(EngineError, "stored_source_changed"):
            self.engine.fetch_source(SOURCE)
        with self.assertRaises(EngineError):
            self.engine.fetch_source("not-ingested")
        posted = []
        self.engine._post = lambda path, body: (posted.append((path, body)), {"results": []})[1]
        self.assertEqual(self.engine.search("Which turn?", "2026-04-02"), {"results": []})
        self.assertEqual(posted, [("/v1/search", {"query": "Which turn?", "top_k": 60, "trace": True, "dry_run": True,
                                                   "reference_time": "2026-04-02T00:00:00Z"})])
        self.engine.search("Which turn?", "2026-04-02", top_k=5)
        self.assertEqual(posted[-1][1]["top_k"], 5)
        for bad in (("Which turn?", "2026-4-2"), ("", "2026-04-02"), (" ", "2026-04-02")):
            with self.subTest(bad=bad), self.assertRaises(EngineError):
                self.engine.search(*bad)
        self.engine._post = lambda path, body: {"error": "x"}
        with self.assertRaises(EngineError):
            self.engine.search("Which turn?", "2026-04-02")
        self.assertEqual(receipt.fragment_ids, [f"{SOURCE}:f0", f"{SOURCE}:f1"])


if __name__ == "__main__":
    unittest.main()
