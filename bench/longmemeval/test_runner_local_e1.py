"""E1 fragments-arm adapter tests against a fake MCP/HTTP transport; no engine."""
import argparse
import hashlib
import json
from pathlib import Path
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

from representations import _record
import runner_local_e1
from runner_local_e1 import (LocalEngineAdapterE1, e1_settings, effective_config_e1,
                             fragment_record_id, in_scope_e1, ingest_arguments,
                             parse_ingest_result, question_payload_e1, render_transcript,
                             retrieve_e1, session_records)
from validate import ValidationError, build_manifest

SOURCE_A = "11111111-2222-3333-4444-555555555555"
SOURCE_B = "66666666-7777-8888-9999-aaaaaaaaaaaa"
EVIDENCE = {"author": "user", "conversation_date": "2023-05-20", "event_date": None,
            "event_date_provenance": None, "source_id": SOURCE_A, "byte_start": 0, "byte_end": 11}
STAGES = ("ranked_candidates", "after_soft_delete", "after_filters_and_dedup", "response")


def question():
    return {"question_id": "synthetic", "question": "Which bike?", "answer": "GOLD",
            "question_type": "single-session-user",
            "haystack_session_ids": ["s1", "s2"],
            "haystack_dates": ["2023/05/20 (Sat) 02:21", "2023/06/01 (Thu) 10:00"],
            "haystack_sessions": [
                [{"role": "user", "content": "I ride a gravel bike.", "has_answer": True},
                 {"role": "assistant", "content": "Nice.\nWhere?", "has_answer": False}],
                [{"role": "user", "content": "Unrelated", "has_answer": False}]]}


def ingest_text(source_id, fragments):
    return (f"Stored source transcript (id: {source_id}). Knowledge extraction disabled. "
            f"Stored {fragments} fragments.")


def tool_result(text, is_error=False):
    result = {"content": [{"type": "text", "text": text}]}
    if is_error:
        result["isError"] = True
    return result


def fake_adapter(settings=None):
    adapter = LocalEngineAdapterE1.__new__(LocalEngineAdapterE1)
    adapter.settings = settings or {"capture_context": False, "top_k": 5,
                                    "_e1": {"provenance_role": "claim", "token_counter": "approx"}}
    adapter.process = SimpleNamespace(poll=lambda: None)
    adapter.mapping, adapter.fragment_counts = {}, {}
    adapter.session_id, adapter.rpc_id = None, 0
    adapter.token, adapter.endpoint = "t" * 64, "http://127.0.0.1:39999"
    return adapter


class FakeMcp:
    """Records every JSON-RPC body/headers; answers initialize and memory_ingest."""

    def __init__(self, texts):
        self.texts, self.calls = list(texts), []

    def __call__(self, body, session_id=None):
        self.calls.append((json.loads(json.dumps(body)), session_id))
        if body["method"] == "initialize":
            return ({"jsonrpc": "2.0", "id": body["id"],
                     "result": {"protocolVersion": "2025-03-26", "capabilities": {}}}, "sess-1")
        if session_id != "sess-1":
            return ({"jsonrpc": "2.0", "id": body["id"],
                     "error": {"code": -32600, "message": "Session not initialized"}}, None)
        text = self.texts.pop(0)
        return ({"jsonrpc": "2.0", "id": body["id"], "result": tool_result(text)}, "sess-1")


class TranscriptAndRecordTests(unittest.TestCase):
    def test_transcript_is_role_colon_content_lines_joined_by_newline(self):
        session = question()["haystack_sessions"][0]
        self.assertEqual(render_transcript(session),
                         "user: I ride a gravel bike.\nassistant: Nice.\nWhere?")
        for bad in [[], [{"role": "", "content": "x"}], [{"role": "user:", "content": "x"}],
                    [{"role": " user", "content": "x"}], [{"role": "user", "content": None}],
                    [{"role": None, "content": "x"}], [{"role": "system", "content": "x"}],
                    [{"role": "Anna", "content": "x"}]]:
            with self.subTest(bad=bad), self.assertRaises(ValueError):
                render_transcript(bad)

    def test_session_records_use_v1_record_hash_and_dataset_date_verbatim(self):
        records, query = question_payload_e1(question(), {})
        self.assertEqual(query, "Which bike?")
        self.assertEqual([r["kind"] for r in records], ["session", "session"])
        self.assertEqual(records[0]["content"], "user: I ride a gravel bike.\nassistant: Nice.\nWhere?")
        self.assertEqual(records[0]["date"], "2023/05/20 (Sat) 02:21")
        self.assertIsNone(records[0]["role"])
        self.assertEqual(records[0]["sources"], [{"session_id": "s1", "turn_index": 0},
                                                  {"session_id": "s1", "turn_index": 1}])
        expected = _record("synthetic", "session", records[0]["content"], records[0]["sources"],
                           {"role": None, "date": "2023/05/20 (Sat) 02:21"})
        self.assertEqual(records[0]["record_id"], expected["record_id"])
        self.assertEqual(len({r["record_id"] for r in records}), 2)
        serialized = json.dumps(records)
        self.assertNotIn("GOLD", serialized)
        self.assertNotIn("has_answer", serialized)
        self.assertNotIn("single-session-user", serialized)

    def test_gold_edits_do_not_change_session_ids(self):
        base = [r["record_id"] for r in question_payload_e1(question(), {})[0]]
        edited = question()
        edited["answer"] = "OTHER"
        edited["haystack_sessions"][0][0]["has_answer"] = False
        self.assertEqual([r["record_id"] for r in question_payload_e1(edited, {})[0]], base)

    def test_occurrence_v1_excludes_exact_empty_turns_and_keeps_repeats(self):
        row = question()
        row["haystack_session_ids"] = ["s1", "s1"]
        row["haystack_sessions"][0].insert(1, {"role": "assistant", "content": ""})
        with self.assertRaises(ValueError):
            session_records(row)
        records = session_records(row, "occurrence-v1")
        self.assertEqual(records[0]["content"], "user: I ride a gravel bike.\nassistant: Nice.\nWhere?")
        self.assertEqual([s["turn_index"] for s in records[0]["sources"]], [0, 2])
        self.assertEqual([s["session_occurrence"] for s in records[0]["sources"]], [0, 0])
        self.assertEqual(records[1]["sources"][0]["session_occurrence"], 1)

    def test_ingest_arguments_normalise_the_dataset_date_or_omit_it(self):
        record = session_records(question())[0]
        self.assertEqual(ingest_arguments(record),
                         {"content": record["content"], "conversation_date": "2023-05-20"})
        self.assertEqual(ingest_arguments(dict(record, date="May 20")), {"content": record["content"]})


class ConfigTests(unittest.TestCase):
    def artifacts(self, root, template_text):
        files = {"binary": root / "loomem-server", "template": root / "config.toml"}
        root.mkdir(parents=True, exist_ok=True)
        model = root / "model"
        model.mkdir(exist_ok=True)
        files["binary"].write_bytes(b"\x7fELF")
        files["template"].write_text(template_text)
        (model / "model.onnx").write_bytes(b"onnx")
        (model / "tokenizer.json").write_text("{}")
        digest = lambda path: hashlib.sha256(Path(path).read_bytes()).hexdigest()
        return {"binary": str(files["binary"]), "binary_sha256": digest(files["binary"]),
                "template": str(files["template"]), "template_sha256": digest(files["template"]),
                "model_dir": str(model), "model_onnx_sha256": digest(model / "model.onnx"),
                "tokenizer_sha256": digest(model / "tokenizer.json")}

    def test_generated_config_turns_fragments_and_evidence_on_with_extraction_off(self):
        template = ("[server]\nport = 1\n\n[search]\ntop_k = 10\nevidence_fields = false\n\n"
                    "[knowledge_extraction]\nenabled = true\n\n[fragments]\nenabled = false\n"
                    "max_fragment_tokens = 400\nprovenance_role = \"claim\"\n"
                    "token_counter = \"approx\"\n")
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            settings = {"endpoint": "http://127.0.0.1:39999", "local_engine": self.artifacts(root, template),
                        "_e1": {"provenance_role": "evidence", "token_counter": "tiktoken"}}
            text = effective_config_e1(settings, root / "work")
            fragments = text.split("[fragments]")[1].split("[")[0]
            self.assertIn("enabled = true", fragments)
            self.assertIn('provenance_role = "evidence"', fragments)
            self.assertIn('token_counter = "tiktoken"', fragments)
            self.assertIn("max_fragment_tokens = 400", fragments)
            search = text.split("[search]")[1].split("\n[")[0]
            self.assertIn("evidence_fields = true", search)
            extraction = text.split("[knowledge_extraction]")[1].split("[")[0]
            self.assertIn("enabled = false", extraction)
            self.assertNotIn("enabled = true", extraction)
            self.assertIn('embedding_provider = "local"', text)
            self.assertIn('api_key = ""', text)
            # A template without a [fragments] section gets one appended.
            settings["local_engine"] = self.artifacts(root / "other", "[server]\nport = 1\n")
            appended = effective_config_e1(settings, root / "work2")
            self.assertIn("[fragments]\nenabled = true\n", appended)

    def test_e1_settings_are_strict(self):
        with tempfile.TemporaryDirectory() as directory:
            config = Path(directory) / "config.json"
            for good in [{"runner_e1": {"provenance_role": "cue", "token_counter": "approx"}},
                         {"runner_e1": {"provenance_role": "claim", "token_counter": "embedder"}}]:
                config.write_text(json.dumps(good))
                with self.subTest(good=good):
                    self.assertEqual(e1_settings(config), good["runner_e1"])
            for bad in [{}, {"runner_e1": {"provenance_role": "cue"}},
                        {"runner_e1": {"provenance_role": "claim", "token_counter": "chars"}},
                        {"runner_e1": {"provenance_role": "fact", "token_counter": "approx"}},
                        {"runner_e1": {"provenance_role": "claim", "token_counter": "approx", "x": 1}}]:
                config.write_text(json.dumps(bad))
                with self.subTest(bad=bad), self.assertRaises(ValidationError):
                    e1_settings(config)


class IngestResultTests(unittest.TestCase):
    def test_strict_parse(self):
        self.assertEqual(parse_ingest_result(tool_result(ingest_text(SOURCE_A, 3))), (SOURCE_A, 3))
        self.assertEqual(parse_ingest_result(tool_result(ingest_text(SOURCE_A, 0))), (SOURCE_A, 0))
        for text in [ingest_text(SOURCE_A, 3) + " ", " " + ingest_text(SOURCE_A, 3),
                     ingest_text(SOURCE_A, "03"), ingest_text("not-a-uuid", 3),
                     f"Stored source transcript (id: {SOURCE_A}). Knowledge extraction disabled.",
                     f"Stored source transcript (id: {SOURCE_A}). Knowledge extraction disabled. "
                     "Stored 0 fragments (segmentation failed: bad).",
                     f"Stored source transcript (id: {SOURCE_A}). Knowledge extraction disabled. "
                     "Stored 2 fragments, 1 fragment writes failed.",
                     f"Source stored (id: {SOURCE_A}). Extracted 2 facts from conversation, stored 2, "
                     "skipped 0. Stored 3 fragments.",
                     f"Source persistence failed (id: {SOURCE_A}). Source may exist; no extraction or "
                     "fact writes started."]:
            with self.subTest(text=text), self.assertRaises(ValidationError):
                parse_ingest_result(tool_result(text))
        with self.assertRaises(ValidationError):
            parse_ingest_result(tool_result(ingest_text(SOURCE_A, 3), is_error=True))
        with self.assertRaises(ValidationError):
            parse_ingest_result({"content": [{"type": "image", "data": ""}]})


class AdapterTests(unittest.TestCase):
    def test_ingest_is_one_memory_ingest_call_per_session_through_mcp(self):
        adapter = fake_adapter()
        mcp = FakeMcp([ingest_text(SOURCE_A, 3), ingest_text(SOURCE_B, 1)])
        adapter.mcp_request = mcp
        records = session_records(question())
        result = adapter.request("/ingest", {"scope": "x", "records": records})
        self.assertEqual(result, {"status": "complete",
                                  "accepted_ids": [r["record_id"] for r in records]})
        self.assertEqual([body["method"] for body, _ in mcp.calls],
                         ["initialize", "tools/call", "tools/call"])
        self.assertEqual([session for _, session in mcp.calls], [None, "sess-1", "sess-1"])
        first = mcp.calls[1][0]
        self.assertEqual(first["jsonrpc"], "2.0")
        self.assertEqual(first["params"], {"name": "memory_ingest", "arguments": {
            "content": "user: I ride a gravel bike.\nassistant: Nice.\nWhere?",
            "conversation_date": "2023-05-20"}})
        self.assertEqual(mcp.calls[2][0]["params"]["arguments"]["conversation_date"], "2023-06-01")
        self.assertEqual(adapter.mapping, {SOURCE_A: records[0]["record_id"],
                                           SOURCE_B: records[1]["record_id"]})
        self.assertEqual(adapter.fragment_counts, {SOURCE_A: 3, SOURCE_B: 1})
        self.assertEqual(adapter.expected_chunks(), 6)
        self.assertNotIn("GOLD", json.dumps(mcp.calls))
        self.assertNotIn("has_answer", json.dumps(mcp.calls))

    def test_ingest_failures(self):
        adapter = fake_adapter()
        adapter.mcp_request = FakeMcp([ingest_text(SOURCE_A, 3), ingest_text(SOURCE_A, 1)])
        records = session_records(question())
        with self.assertRaises(ValidationError):
            adapter.request("/ingest", {"scope": "x", "records": records})
        adapter = fake_adapter()
        adapter.mcp_request = FakeMcp([ingest_text(SOURCE_A, 3)])
        with self.assertRaises(ValidationError):
            adapter.request("/ingest", {"scope": "x", "records": [dict(records[0], kind="source")]})
        adapter = fake_adapter()
        adapter.mcp_request = lambda body, session_id=None: (
            {"jsonrpc": "2.0", "id": body["id"], "result": {"protocolVersion": "x"}}, None)
        with self.assertRaises(ValidationError):
            adapter.request("/ingest", {"scope": "x", "records": records[:1]})
        adapter = fake_adapter()
        adapter.mcp_request = lambda body, session_id=None: (
            {"jsonrpc": "2.0", "id": body["id"] + 1, "result": {"protocolVersion": "x"}}, "s")
        with self.assertRaises(ValidationError):
            adapter.request("/ingest", {"scope": "x", "records": records[:1]})

    def test_readiness_counts_sources_plus_fragments(self):
        adapter = fake_adapter()
        records = session_records(question())
        adapter.mapping = {SOURCE_A: records[0]["record_id"], SOURCE_B: records[1]["record_id"]}
        adapter.fragment_counts = {SOURCE_A: 3, SOURCE_B: 1}
        counts = iter([2, 6])

        def engine_request(path, payload=None):
            if path == "/v1/status":
                adapter.current = next(counts)
                return {"config_summary": {"embeddings_count": adapter.current,
                                           "tantivy_docs": adapter.current}}
            self.assertEqual(path, "/v1/health/index-sync")
            return {"rocksdb_chunks": adapter.current, "tantivy_docs": adapter.current}
        adapter.engine_request = engine_request
        ready = adapter.request("/ready", {"scope": "x", "expected_ids": [r["record_id"] for r in records]})
        self.assertEqual(ready, {"ready": True, "pending_jobs": 0, "indexes_ready": True,
                                 "indexed_ids": [records[0]["record_id"], records[1]["record_id"]]})
        with self.assertRaises(ValidationError):
            adapter.request("/ready", {"scope": "x", "expected_ids": [records[0]["record_id"]]})

    def test_readiness_times_out_when_fragments_never_index(self):
        adapter = fake_adapter()
        adapter.mapping, adapter.fragment_counts = {SOURCE_A: "r"}, {SOURCE_A: 2}
        adapter.engine_request = lambda path, payload=None: {
            "config_summary": {"embeddings_count": 1, "tantivy_docs": 1},
            "rocksdb_chunks": 1, "tantivy_docs": 1}
        cls = runner_local_e1.LocalEngineAdapterE1
        original = (cls.READY_DEADLINE_SECONDS, cls.READY_SECONDS_PER_CHUNK, cls.READY_POLL_SECONDS)
        cls.READY_DEADLINE_SECONDS, cls.READY_SECONDS_PER_CHUNK, cls.READY_POLL_SECONDS = 0.05, 0.0, 0.01
        try:
            with self.assertRaises(ValidationError):
                adapter.request("/ready", {"scope": "x", "expected_ids": ["r"]})
        finally:
            cls.READY_DEADLINE_SECONDS, cls.READY_SECONDS_PER_CHUNK, cls.READY_POLL_SECONDS = original

    def test_readiness_deadline_scales_with_expected_chunks(self):
        adapter = fake_adapter()
        adapter.mapping, adapter.fragment_counts = {SOURCE_A: "r", SOURCE_B: "s"}, {SOURCE_A: 300, SOURCE_B: 358}
        cls = runner_local_e1.LocalEngineAdapterE1
        self.assertEqual(adapter.expected_chunks(), 660)
        self.assertEqual(adapter.ready_deadline_seconds(adapter.expected_chunks()),
                         cls.READY_DEADLINE_SECONDS + cls.READY_SECONDS_PER_CHUNK * 660)
        self.assertGreaterEqual(adapter.ready_deadline_seconds(0), 45)

    def test_search_maps_source_and_fragment_ids_and_rejections(self):
        adapter = fake_adapter()
        adapter.settings["capture_context"] = True
        adapter.mapping = {SOURCE_A: "a" * 64, SOURCE_B: "b" * 64}
        adapter.fragment_counts = {SOURCE_A: 3, SOURCE_B: 1}
        stages = [[SOURCE_A + ":f1", SOURCE_A, SOURCE_B + ":f0", SOURCE_A + ":f3"],
                  [SOURCE_A + ":f1", SOURCE_A, SOURCE_B + ":f0"],
                  [SOURCE_A + ":f1", SOURCE_A], [SOURCE_A + ":f1", SOURCE_A]]
        adapter.engine_request = lambda path, payload=None: {
            "results": [{"id": SOURCE_A + ":f1", "content": "user: I ride a gravel bike.",
                         "evidence": dict(EVIDENCE)},
                        {"id": SOURCE_A, "content": "user: I ride a gravel bike.\nassistant: Nice.\nWhere?"}],
            "trace_metadata": {
                "evidence": {"schema_version": 1, "coverage": "post_rank_to_response",
                             "stages": [{"stage": n, "chunk_ids": ids} for n, ids in zip(STAGES, stages)]},
                "rejections": [
                    {"chunk_id": SOURCE_A + ":f3", "stage": "after_soft_delete", "reason": "soft_deleted"},
                    {"chunk_id": SOURCE_B + ":f0", "stage": "after_filters_and_dedup",
                     "reason": "content_prefix_dedup"},
                    {"chunk_id": "cccccccc-cccc-cccc-cccc-cccccccccccc:f0", "stage": "response",
                     "reason": "truncated_top_k"}]}}
        result = adapter.request("/search", {"query": "q", "top_k": 2, "trace": True,
                                             "capture_context": True})
        self.assertEqual(result["retrieved_ids"], [fragment_record_id("a" * 64, 1), "a" * 64])
        self.assertEqual(result["trace_metadata"]["evidence"]["stages"][0]["chunk_ids"],
                         ["a" * 64 + ":f1", "a" * 64, "b" * 64 + ":f0", "a" * 64 + ":f3"])
        self.assertEqual(result["rejections"], [
            {"chunk_id": "a" * 64 + ":f3", "stage": "after_soft_delete", "reason": "soft_deleted"},
            {"chunk_id": "b" * 64 + ":f0", "stage": "after_filters_and_dedup",
             "reason": "content_prefix_dedup"}])
        self.assertEqual(result["retrieved_records_v2"][0],
                         {"record_id": "a" * 64 + ":f1", "content": "user: I ride a gravel bike.", **EVIDENCE})
        self.assertEqual(result["retrieved_records_v2"][1]["source_id"], None)
        self.assertEqual(result["retrieved_records"][1]["record_id"], "a" * 64)
        for bad in ["dddddddd-dddd-dddd-dddd-dddddddddddd", SOURCE_A + ":f01", SOURCE_A + ":g1",
                    SOURCE_A + ":f", "x:f1"]:
            with self.subTest(bad=bad), self.assertRaises(KeyError):
                adapter.map_id(bad)

    def test_fetch_records_v2_resolves_fragment_ids(self):
        adapter = fake_adapter()
        adapter.settings["capture_context"] = True
        adapter.mapping = {SOURCE_A: "a" * 64}
        requests = []

        def engine_request(path, payload=None):
            requests.append(path)
            engine_id = path.rsplit("/", 1)[1].replace("%3A", ":")
            return {"id": engine_id, "content": "stored", "evidence": dict(EVIDENCE)}
        adapter.engine_request = engine_request
        records = adapter.fetch_records_v2(["a" * 64 + ":f2", "a" * 64])
        self.assertEqual([r["record_id"] for r in records], ["a" * 64 + ":f2", "a" * 64])
        self.assertEqual(requests, ["/api/memories/" + SOURCE_A + "%3Af2", "/api/memories/" + SOURCE_A])
        with self.assertRaises(ValidationError):
            adapter.fetch_records_v2(["b" * 64 + ":f0"])

    def test_scope_predicate(self):
        ids = {"a" * 64}
        self.assertTrue(in_scope_e1("a" * 64, ids))
        self.assertTrue(in_scope_e1("a" * 64 + ":f12", ids))
        self.assertFalse(in_scope_e1("b" * 64 + ":f0", ids))
        self.assertFalse(in_scope_e1("a" * 64 + ":f01", ids))
        self.assertFalse(in_scope_e1(None, ids))


class DrivenAdapter(LocalEngineAdapterE1):
    """Whole E1 flow against fakes: no process, no socket, no files."""

    def __init__(self, response, texts):
        self.settings = {"capture_context": True, "top_k": 5,
                         "_e1": {"provenance_role": "claim", "token_counter": "approx"}}
        self.process = SimpleNamespace(poll=lambda: None)
        self.mapping, self.fragment_counts = {}, {}
        self.session_id, self.rpc_id = None, 0
        self.token, self.endpoint = "t" * 64, "http://127.0.0.1:39999"
        self.mcp = FakeMcp(texts)
        self.response, self.closed, self.calls = response, 0, []

    def identity(self):
        self.calls.append("identity")
        self.mapping, self.fragment_counts, self.session_id = {}, {}, None

    def mcp_request(self, body, session_id=None):
        return self.mcp(body, session_id)

    def engine_request(self, path, payload=None):
        self.calls.append(path)
        if path == "/v1/status":
            return {"config_summary": {"embeddings_count": self.expected_chunks(),
                                       "tantivy_docs": self.expected_chunks()}}
        if path == "/v1/health/index-sync":
            return {"rocksdb_chunks": self.expected_chunks(), "tantivy_docs": self.expected_chunks()}
        self.assertions = payload
        return json.loads(json.dumps(self.response))

    def close(self):
        self.closed += 1


class RetrieveE1Tests(unittest.TestCase):
    def response(self):
        stages = [[SOURCE_A + ":f0", SOURCE_B], [SOURCE_A + ":f0", SOURCE_B],
                  [SOURCE_A + ":f0"], [SOURCE_A + ":f0"]]
        return {"results": [{"id": SOURCE_A + ":f0", "content": "user: I ride a gravel bike.",
                             "evidence": dict(EVIDENCE)}],
                "trace_metadata": {
                    "evidence": {"schema_version": 1, "coverage": "post_rank_to_response",
                                 "stages": [{"stage": n, "chunk_ids": ids} for n, ids in zip(STAGES, stages)]},
                    "rejections": [{"chunk_id": SOURCE_B, "stage": "after_filters_and_dedup",
                                    "reason": "content_prefix_dedup"}]}}

    def test_full_flow_yields_v2_row_with_rejections_and_context(self):
        adapter = DrivenAdapter(self.response(), [ingest_text(SOURCE_A, 2), ingest_text(SOURCE_B, 1)])
        artifacts = []
        row = retrieve_e1(adapter, question(), "m" * 64, context_sink=artifacts.append)
        records = session_records(question())
        a, b = records[0]["record_id"], records[1]["record_id"]
        self.assertEqual(row["status"], "retrieval_complete")
        self.assertEqual(row["retrieved_ids"], [a + ":f0"])
        self.assertEqual(row["ingested_ids"], [a, b])
        self.assertEqual(row["evidence"]["stages"][0]["chunk_ids"], [a + ":f0", b])
        self.assertEqual(row["rejections"], [{"chunk_id": b, "stage": "after_filters_and_dedup",
                                              "reason": "content_prefix_dedup"}])
        self.assertEqual(adapter.closed, 1)
        self.assertEqual(adapter.calls[0], "identity")
        self.assertEqual(adapter.assertions["dry_run"], True)
        self.assertEqual(artifacts[0]["schema_version"], 2)
        self.assertEqual(artifacts[0]["records"][0]["record_id"], a + ":f0")
        self.assertEqual(artifacts[0]["records"][0]["conversation_date"], "2023-05-20")
        self.assertEqual(row["context_sha256"], runner_local_e1.canonical_hash(artifacts[0]))
        serialized = json.dumps(row) + json.dumps(adapter.mcp.calls) + json.dumps(adapter.assertions)
        self.assertNotIn("GOLD", serialized)
        self.assertNotIn("has_answer", serialized)
        self.assertNotIn("single-session-user", serialized)

    def test_foreign_or_malformed_results_fail_at_retrieval_and_close(self):
        response = self.response()
        response["results"][0]["id"] = "cccccccc-cccc-cccc-cccc-cccccccccccc:f0"
        adapter = DrivenAdapter(response, [ingest_text(SOURCE_A, 2), ingest_text(SOURCE_B, 1)])
        row = retrieve_e1(adapter, question(), "m" * 64, context_sink=lambda _: None)
        self.assertEqual(row, {"question_id": "synthetic", "manifest_sha256": "m" * 64,
                               "status": "failed", "failure_stage": "retrieval", "rejections": []})
        self.assertEqual(adapter.closed, 1)
        adapter = DrivenAdapter(self.response(), [ingest_text(SOURCE_A, 2), ingest_text(SOURCE_A, 1)])
        row = retrieve_e1(adapter, question(), "m" * 64, context_sink=lambda _: None)
        self.assertEqual(row["failure_stage"], "ingest")
        adapter = DrivenAdapter(self.response(), [ingest_text(SOURCE_A, 2), ingest_text(SOURCE_B, 1)])
        self.assertEqual(retrieve_e1(adapter, question(), "m" * 64)["failure_stage"], "prepare")


class RunE1Tests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.args = argparse.Namespace(
            dataset=self.root / "dataset.json", config=self.root / "config.json",
            manifest=self.root / "manifest.json", trace=self.root / "trace.jsonl",
            contexts=None, dry_run=False)

    def freeze(self, e1=None, capture=False):
        artifacts = ConfigTests.artifacts(self, self.root / "engine", "[server]\nport = 1\n")
        settings = {"endpoint": "http://127.0.0.1:39999", "instance_id": "synthetic",
                    "data_dir": str(self.root / "db"), "run_id": "synthetic-run", "top_k": 5,
                    "mode": "raw", "local_engine": artifacts}
        if capture:
            settings["capture_context"] = True
        config = {key: "a" * size for key, size in [("engine_sha", 40), ("harness_sha", 40),
                  ("reader_prompt_sha256", 64), ("judge_prompt_sha256", 64),
                  ("execution_config_sha256", 64)]}
        config.update(reader_model="disabled", judge_model="disabled", embedding_model="synthetic",
                      seed=0, context_budget_tokens=100, runner=settings)
        if e1 is not None:
            config["runner_e1"] = e1
        self.args.dataset.write_text(json.dumps([question()]))
        self.args.config.write_text(json.dumps(config))
        self.args.manifest.write_text(json.dumps(build_manifest(self.args.dataset, self.args.config)))

    def test_dry_run_validates_projections_without_network_or_writes(self):
        self.freeze({"provenance_role": "cue", "token_counter": "approx"})
        self.args.dry_run = True
        with patch.object(LocalEngineAdapterE1, "__init__", side_effect=AssertionError("launch")):
            summary = runner_local_e1.run_e1(self.args)
        self.assertEqual(summary["network_calls"], 0)
        self.assertEqual(summary["arm"], "E1")
        self.assertFalse(self.args.trace.exists())

    def test_missing_or_invalid_e1_knobs_reject_the_run(self):
        self.freeze()
        self.args.dry_run = True
        with self.assertRaises(ValidationError):
            runner_local_e1.run_e1(self.args)
        self.freeze({"provenance_role": "claim", "token_counter": "chars"})
        with self.assertRaises(ValidationError):
            runner_local_e1.run_e1(self.args)

    def test_run_writes_v2_sidecar_and_contexts_and_is_create_only(self):
        self.freeze({"provenance_role": "claim", "token_counter": "approx"}, capture=True)
        self.args.contexts = self.root / "contexts"
        response = RetrieveE1Tests.response(self)

        def driven(settings):
            adapter = DrivenAdapter(response, [ingest_text(SOURCE_A, 2), ingest_text(SOURCE_B, 1)])
            adapter.settings = settings
            return adapter
        with patch.object(runner_local_e1, "LocalEngineAdapterE1", driven):
            summary = runner_local_e1.run_e1(self.args)
        self.assertEqual(summary["failed"], 0)
        rows = [json.loads(line) for line in self.args.trace.read_text().splitlines()]
        self.assertEqual(len(rows), 1)
        self.assertEqual(rows[0]["status"], "retrieval_complete")
        self.assertEqual(len(rows[0]["rejections"]), 1)
        artifact = list(self.args.contexts.glob("*.json"))
        self.assertEqual(len(artifact), 1)
        self.assertEqual(json.loads(artifact[0].read_text())["schema_version"], 2)
        self.assertNotIn("GOLD", self.args.trace.read_text() + artifact[0].read_text())
        with patch.object(runner_local_e1, "LocalEngineAdapterE1", driven):
            with self.assertRaises(ValidationError):
                runner_local_e1.run_e1(self.args)


if __name__ == "__main__":
    unittest.main()
