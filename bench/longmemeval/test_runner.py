"""Synthetic HTTP fixture only: no existing instance or paid services."""
import argparse
from contextlib import contextmanager
from http.server import BaseHTTPRequestHandler, HTTPServer
import json
from pathlib import Path
import tempfile
import threading
import unittest
from unittest.mock import patch

import runner
import runner_local
from validate import build_manifest, canonical_hash


@contextmanager
def fixture(settings, behavior="ok"):
    requests, ids = [], []

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *_):
            pass

        def do_GET(self):
            self.respond()

        def do_POST(self):
            self.respond()

        def respond(self):
            body = json.loads(self.rfile.read(int(self.headers["Content-Length"]))) if "Content-Length" in self.headers else None
            requests.append((self.path, body))
            if self.path == "/identity":
                result = {key: settings[key] for key in ("instance_id", "data_dir", "run_id")}
                result.update(schema_version=1, dedicated=True, network_policy="offline", paid_calls_enabled=False)
                if behavior == "identity":
                    result["dedicated"] = False
            elif self.path == "/ingest":
                ids[:] = [record["record_id"] for record in body["records"]]
                result = {"status": "complete", "accepted_ids": ids if behavior != "partial" else []}
            elif self.path == "/ready":
                result = {"ready": behavior != "pending", "pending_jobs": 0,
                          "indexes_ready": True, "indexed_ids": ids}
            else:
                result = {"retrieved_ids": ids, "content": "DO_NOT_PERSIST", "answer": "GOLD"}
                if body.get("capture_context") is True:
                    result["retrieved_records"] = [{"record_id": item, "content": "Actual sanitized [REDACTED]"} for item in ids]
                if behavior == "foreign":
                    result["retrieved_ids"] = ["other-question"]
            self.send_response(200)
            self.end_headers()
            self.wfile.write(json.dumps(result).encode())

    server = HTTPServer(("127.0.0.1", 0), Handler)
    settings["endpoint"] = "http://127.0.0.1:" + str(server.server_port)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        yield requests
    finally:
        server.shutdown()
        server.server_close()
        thread.join()


class RunnerTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        root = Path(self.temp.name)
        self.args = argparse.Namespace(dataset=root / "dataset.json", config=root / "config.json",
            manifest=root / "manifest.json", trace=root / "trace.jsonl", dry_run=False, resume=False)
        self.row = {"question_id": "synthetic_abs", "question_type": "single-session-user",
                    "question": "Which color?", "question_date": "2024/01/01 (Mon) 12:34",
                    "answer": "GOLD", "haystack_session_ids": ["s1"],
                    "haystack_dates": ["2024-01-01"], "haystack_sessions": [[{
                        "role": "user", "content": "Synthetic blue widget", "has_answer": True}]]}
        self.settings = {"endpoint": "http://127.0.0.1:1", "instance_id": "synthetic",
                         "data_dir": str(root / "db"), "run_id": "synthetic-run", "top_k": 2, "mode": "raw"}

    def freeze(self):
        config = {key: "a" * size for key, size in [("engine_sha", 40), ("harness_sha", 40),
                  ("reader_prompt_sha256", 64), ("judge_prompt_sha256", 64), ("execution_config_sha256", 64)]}
        config.update(reader_model="disabled", judge_model="disabled", embedding_model="synthetic",
                      seed=0, context_budget_tokens=100, runner=self.settings)
        self.args.dataset.write_text(json.dumps([self.row]))
        self.args.config.write_text(json.dumps(config))
        self.args.manifest.write_text(json.dumps(build_manifest(self.args.dataset, self.args.config)))

    def test_end_to_end_and_resume_are_label_blind(self):
        with fixture(self.settings) as requests:
            self.freeze()
            self.assertEqual(runner.run(self.args)["failed"], 0)
            traffic = json.dumps(requests)
            for forbidden in ("GOLD", "has_answer", "synthetic_abs"):
                self.assertNotIn(forbidden, traffic)
            self.assertNotIn("DO_NOT_PERSIST", self.args.trace.read_text())
            before = len(requests)
            self.args.resume = True
            self.assertEqual(runner.run(self.args)["failed"], 0)
            self.assertEqual(len(requests), before)

    def test_e1_reference_time_is_utc_hash_bound_and_opt_in(self):
        self.settings.update(operation_policy="E1-C", debug_operation=True)
        with fixture(self.settings) as requests:
            self.freeze()
            self.assertEqual(runner.run(self.args)["failed"], 0)
        search = next(body for path, body in requests if path == "/search")
        self.assertEqual(search["reference_time"], "2024-01-01T12:34:00Z")
        self.assertEqual(search["operation_policy"], "E1-C")
        self.assertIs(search["debug_operation"], True)
        trace = json.loads(self.args.trace.read_text())
        self.assertEqual(trace["operation_policy"], "E1-C")
        self.assertEqual(trace["reference_time_sha256"],
                         canonical_hash("2024-01-01T12:34:00Z"))

    def test_malformed_reference_time_disables_date_treatment(self):
        self.row["question_date"] = "2024/01/01 (Tue) 12:34"
        self.settings["operation_policy"] = "E1-D"
        with fixture(self.settings) as requests:
            self.freeze()
            self.assertEqual(runner.run(self.args)["failed"], 0)
        search = next(body for path, body in requests if path == "/search")
        self.assertNotIn("reference_time", search)
        self.assertEqual(json.loads(self.args.trace.read_text())["reference_time_sha256"],
                         canonical_hash(None))

    def test_e1_f_does_not_propagate_question_clock(self):
        self.settings["operation_policy"] = "E1-F"
        with fixture(self.settings) as requests:
            self.freeze()
            self.assertEqual(runner.run(self.args)["failed"], 0)
        search = next(body for path, body in requests if path == "/search")
        self.assertNotIn("reference_time", search)
        self.assertNotIn("reference_time_sha256", json.loads(self.args.trace.read_text()))

    def test_e2_policies_validate_without_legacy_local_adapter(self):
        for policy in sorted(runner.E2_POLICIES):
            with self.subTest(policy=policy):
                self.settings["operation_policy"] = policy
                self.freeze()
                self.assertEqual(runner.settings_from(self.args.config)["operation_policy"], policy)
                with self.assertRaises(ValueError):
                    runner.validate_operation_settings({**self.settings, "local_engine": {}})

    def test_e2_resume_rejected_before_adapter_or_write(self):
        for policy in sorted(runner.E2_POLICIES):
            with self.subTest(policy=policy):
                self.settings["operation_policy"] = policy
                self.freeze()
                self.args.resume = True
                with patch.object(runner, "Adapter", side_effect=AssertionError("adapter")):
                    with self.assertRaises(ValueError):
                        runner.run(self.args)
                self.assertFalse(self.args.trace.exists())

    def test_failed_stages_never_search(self):
        for behavior, stage in [("identity", "identity"), ("partial", "ingest"), ("pending", "readiness")]:
            with self.subTest(behavior=behavior), fixture(self.settings, behavior) as requests:
                self.freeze()
                self.args.trace.unlink(missing_ok=True)
                self.assertEqual(runner.run(self.args)["failed"], 1)
                self.assertEqual(json.loads(self.args.trace.read_text())["failure_stage"], stage)
                self.assertNotIn("/search", [path for path, _ in requests])

    def test_foreign_retrieval_rejected(self):
        with fixture(self.settings, "foreign"):
            self.freeze()
            self.assertEqual(runner.run(self.args)["failed"], 1)

    def test_dry_run_never_networks_or_writes(self):
        self.freeze()
        self.args.dry_run = True
        with patch.object(runner.Adapter, "request", side_effect=AssertionError("network")):
            self.assertEqual(runner.run(self.args)["network_calls"], 0)
        self.assertFalse(self.args.trace.exists())

    def test_loopback_only(self):
        for endpoint in ("https://127.0.0.1:99", "http://localhost:99", "http://example.com:99",
                         "http://127.0.0.1", "http://127.0.0.1:99/a", "http://u@127.0.0.1:99"):
            with self.subTest(endpoint=endpoint), self.assertRaises(ValueError):
                runner.local_endpoint(endpoint)

    def test_redirect_rejected(self):
        with self.assertRaises(ValueError):
            runner.NoRedirect().redirect_request(None, None, 302, "", {}, "https://example.com")

    def test_mismatched_resume_rejected(self):
        self.freeze()
        self.args.resume = True
        self.args.trace.write_text(json.dumps({"question_id": self.row["question_id"],
            "manifest_sha256": "bad", "status": "retrieval_complete", "retrieved_ids": []}) + "\n")
        with self.assertRaises(ValueError):
            runner.run(self.args)

    def test_false_completed_resume_rejected(self):
        self.freeze()
        self.args.resume = True
        self.args.trace.write_text(json.dumps({"question_id": self.row["question_id"],
            "manifest_sha256": canonical_hash(build_manifest(self.args.dataset, self.args.config)),
            "status": "retrieval_complete", "retrieved_ids": ["unknown"]}) + "\n")
        with self.assertRaises(ValueError):
            runner.run(self.args)

    def test_bad_trace_coverage_rejected(self):
        with self.assertRaises(ValueError):
            runner.trace_projection({"trace_metadata": {"evidence": {"schema_version": 1,
                "coverage": "post_rank_to_response", "stages": []}}})

    def test_explicit_context_capture_and_verified_resume(self):
        self.settings["capture_context"] = True
        self.args.contexts = self.args.dataset.parent / "contexts"
        with fixture(self.settings) as requests:
            self.freeze()
            self.assertEqual(runner.run(self.args)["failed"], 0)
            trace = json.loads(self.args.trace.read_text())
            self.assertNotIn("Actual sanitized", self.args.trace.read_text())
            path = self.args.contexts / (trace["context_sha256"] + ".json")
            artifact = runner.load_context_artifact(path, trace, trace["manifest_sha256"])
            self.assertEqual(artifact["records"][0]["content"], "Actual sanitized [REDACTED]")
            self.assertNotIn("Synthetic blue widget", path.read_text())
            self.assertNotIn("GOLD", path.read_text())
            self.args.resume = True
            before = len(requests)
            self.assertEqual(runner.run(self.args)["failed"], 0)
            self.assertEqual(len(requests), before)
            artifact["records"][0]["content"] = "Tampered actual evidence"
            path.write_text(json.dumps(artifact))
            with self.assertRaises(ValueError):
                runner.run(self.args)
            self.assertEqual(len(requests), before)

    def test_context_capture_requires_manifest_opt_in(self):
        self.args.contexts = self.args.dataset.parent / "contexts"
        self.freeze()
        with self.assertRaises(ValueError):
            runner.run(self.args)
        self.assertFalse(self.args.contexts.exists())

    def test_context_capture_requires_explicit_sink(self):
        self.settings["capture_context"] = True
        self.freeze()
        with self.assertRaises(ValueError):
            runner.run(self.args)

    def test_context_record_shape_and_rank_are_exact(self):
        for records in ([{"record_id": "b", "content": "B"}, {"record_id": "a", "content": "A"}],
                        [{"record_id": "a", "content": "A", "answer": "GOLD"}],
                        [{"record_id": "a", "content": None}]):
            with self.subTest(records=records), self.assertRaises(ValueError):
                runner.context_records(records, ["a", "b"])

    def test_local_capture_uses_actual_sanitized_engine_return(self):
        from types import SimpleNamespace
        adapter = runner_local.LocalEngineAdapter.__new__(runner_local.LocalEngineAdapter)
        adapter.process = SimpleNamespace(poll=lambda: None)
        adapter.mapping = {"engine-a": "record-a", "engine-b": "record-b"}
        forwarded = []
        def search_request(path, payload):
            self.assertEqual(path, "/v1/search")
            forwarded.append(payload)
            return {"results": [
                {"id": "engine-b", "content": "Actual sanitized B"},
                {"id": "engine-a", "content": "Actual sanitized A"}]}
        adapter.engine_request = search_request
        result = adapter.request("/search", {"query": "test", "top_k": 2,
                                           "trace": True, "capture_context": True})
        self.assertEqual(result["retrieved_records"], [
            {"record_id": "record-b", "content": "Actual sanitized B"},
            {"record_id": "record-a", "content": "Actual sanitized A"}])
        result = adapter.request("/search", {"query": "test", "top_k": 2, "trace": True})
        self.assertNotIn("retrieved_records", result)
        self.assertEqual(len(forwarded), 2)
        self.assertTrue(all(payload["dry_run"] is True for payload in forwarded))
        self.assertTrue(all(payload["trace"] is True for payload in forwarded))

    def test_owned_adapter_refuses_active_listener_even_with_reuse(self):
        with fixture(self.settings):
            adapter = runner_local.LocalEngineAdapter.__new__(runner_local.LocalEngineAdapter)
            adapter.settings = self.settings
            adapter.endpoint = self.settings["endpoint"]
            adapter.process = None
            adapter.counter = 0
            adapter.invocation = self.args.dataset.parent
            with patch.object(runner_local, "effective_config", return_value=""), \
                    patch.object(runner_local.subprocess, "Popen", side_effect=AssertionError("spawn")), \
                    self.assertRaises(OSError):
                adapter.identity()

    def test_neighbor_fetch_is_scoped_ordered_actual_content(self):
        from types import SimpleNamespace
        adapter = runner_local.LocalEngineAdapter.__new__(runner_local.LocalEngineAdapter)
        adapter.settings = {"capture_context": True}
        adapter.process = SimpleNamespace(poll=lambda: None)
        adapter.mapping = {"engine-a": "record-a", "engine-b": "record-b"}
        requests = []
        def request(path):
            requests.append(path)
            engine_id = path.rsplit("/", 1)[1]
            return {"id": engine_id, "content": "Actual stored " + engine_id}
        adapter.engine_request = request
        self.assertEqual(adapter.fetch_records(["record-b", "record-a"]), [
            {"record_id": "record-b", "content": "Actual stored engine-b"},
            {"record_id": "record-a", "content": "Actual stored engine-a"}])
        self.assertEqual(requests, ["/api/memories/engine-b", "/api/memories/engine-a"])
        with self.assertRaises(ValueError):
            adapter.fetch_records(["another-question"])
        self.assertEqual(len(requests), 2)
        adapter.settings["capture_context"] = False
        with self.assertRaises(ValueError):
            adapter.fetch_records(["record-a"])

    def test_raw_facts_are_manifest_bound(self):
        from validate import sha256
        facts = self.args.dataset.parent / "facts.json"
        facts.write_text(json.dumps({self.row["question_id"]: [{"content": "Synthetic fact", "sources": []}]}))
        self.settings.update(mode="dual", facts_path=str(facts), facts_sha256=sha256(facts))
        self.freeze()
        self.args.dry_run = True
        self.assertEqual(runner.run(self.args)["network_calls"], 0)
        facts.write_text("{}")
        with self.assertRaises(ValueError):
            runner.run(self.args)

    def test_local_config_replace_preserves_neighbors(self):
        template = "[llm]\napi_key = \"secret\" # comment\n[server]\nport = 42\n"
        result = runner_local.replace_setting(template, "llm", "api_key", "")
        self.assertNotIn("secret", result)
        self.assertIn("port = 42", result)
        self.assertIn("enabled = false", runner_local.replace_setting(result, "dream", "enabled", False))


if __name__ == "__main__":
    unittest.main()
