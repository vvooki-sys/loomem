import copy
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

from budget import BudgetLedger
from qa import QARunner, output_identity, aggregate_outputs
from qa_prompts import reader_prompt_sha256, judge_prompt_sha256
from qa_transport import OpenAITransport, complete, validate_model_config
from validate import canonical_hash


def model_config(max_tokens=512):
    return {"model": "gpt-4.1-2025-04-14", "context_window_tokens": 1047576,
            "max_completion_tokens": max_tokens, "input_usd_per_million": "2",
            "output_usd_per_million": "8", "temperature": 0, "seed": 42, "timeout_seconds": 90}


def response(text="synthetic response", finish="stop"):
    return {"model": "gpt-4.1-2025-04-14", "choices": [
        {"message": {"content": text}, "finish_reason": finish}],
        "usage": {"prompt_tokens": 100, "completion_tokens": 8, "total_tokens": 108}}


def fake_pack(records, budget, model):
    return {"context": json.dumps(records), "context_tokens": 12,
            "selected_ids": [r["record_id"] for r in records], "empty": not records}


class QATests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.path = Path(self.tmp.name)
        self.ledger = BudgetLedger(self.path / "ledger.json", "100")

    def invoke(self, transport, config=None, binding=None):
        return complete([{"role": "user", "content": "synthetic question"}],
                        config or model_config(), binding or {"variant": "raw"},
                        self.ledger, self.path / "cache", transport)

    def test_reservation_precedes_network_and_resume_does_not_resubmit(self):
        calls = []
        def fake(payload, timeout):
            self.assertEqual(self.ledger.totals()["pending_count"], 1)
            self.assertGreater(self.ledger.totals()["reserved_usd"], 2)
            calls.append(payload)
            return response()
        first = self.invoke(fake)
        second = self.invoke(fake)
        self.assertEqual(first, second)
        self.assertEqual(len(calls), 1)
        self.assertEqual(self.ledger.totals()["pending_count"], 0)
        self.assertEqual(str(self.ledger.totals()["spent_usd"]), "0.000264")

    def test_uncertain_timeout_keeps_reservation_and_no_retry(self):
        calls = []
        def fake(payload, timeout):
            calls.append(payload)
            raise TimeoutError("SECRET MUST NEVER BE RECORDED")
        first = self.invoke(fake)
        self.assertEqual(first["status"], "failed")
        self.assertEqual(self.invoke(fake), first)
        self.assertEqual(len(calls), 1)
        self.assertEqual(self.ledger.totals()["pending_count"], 1)
        self.assertNotIn("SECRET", json.dumps(first))

    def test_budget_exhaustion_prevents_network(self):
        self.ledger.reserve("other", "99")
        result = self.invoke(lambda *_: self.fail("network must not run"))
        self.assertEqual(result["status"], "failed")
        self.assertEqual(result["error_type"], "BudgetExceeded")

    def test_changed_binding_gets_distinct_request(self):
        first = self.invoke(lambda *_: response())
        second = self.invoke(lambda *_: response(), binding={"variant": "dual"})
        self.assertNotEqual(first["request_sha256"], second["request_sha256"])
        self.assertEqual(self.ledger.totals()["request_count"], 2)

    def test_price_context_and_alias_spoofing_rejected(self):
        for field, value in [("input_usd_per_million", "0"), ("output_usd_per_million", "0.1"),
                             ("context_window_tokens", 1024), ("model", "gpt-4.1")]:
            config = model_config()
            config[field] = value
            with self.assertRaises(ValueError):
                validate_model_config(config)
        self.assertEqual(self.ledger.totals()["request_count"], 0)

    def test_truncated_completion_is_failed_but_usage_settles(self):
        result = self.invoke(lambda *_: response(finish="length"))
        self.assertEqual(result["status"], "failed")
        self.assertEqual(result["error_type"], "IncompleteCompletion")
        self.assertEqual(self.ledger.totals()["pending_count"], 0)

    def test_cached_completion_requires_original_ledger(self):
        self.invoke(lambda *_: response())
        self.ledger = BudgetLedger(self.path / "unrelated-ledger.json", "100")
        with self.assertRaises(Exception):
            self.invoke(lambda *_: self.fail("network must not run"))

    def test_corrupt_cache_fails_before_network(self):
        result = self.invoke(lambda *_: response())
        path = self.path / "cache" / (result["request_sha256"] + ".json")
        entry = json.loads(path.read_text())
        del entry["text"]
        path.write_text(json.dumps(entry))
        with self.assertRaises(ValueError):
            self.invoke(lambda *_: self.fail("network must not run"))

    def test_response_saved_before_settlement_can_resume_without_network(self):
        with patch.object(self.ledger, "settle", side_effect=OSError("synthetic interruption")):
            with self.assertRaises(OSError):
                self.invoke(lambda *_: response())
        self.assertEqual(self.ledger.totals()["pending_count"], 1)
        result = self.invoke(lambda *_: self.fail("network must not repeat"))
        self.assertEqual(result["status"], "completed")
        self.assertEqual(self.ledger.totals()["pending_count"], 0)

    def test_incremental_output_aggregation_and_identity(self):
        output = self.path / "out"
        output_identity(output, "manifest-a")
        rows = [{"question_id": qid, "question_type": "single-session-user"}
                for qid in ("q1", "q2", "q3")]
        for row in rows[:2]:
            record = {**row, "manifest_sha256": "manifest-a", "status": "completed", "correct": True}
            (output / (canonical_hash(row["question_id"]) + ".json")).write_text(json.dumps(record))
        self.assertEqual(len(aggregate_outputs(output, rows, "manifest-a")), 2)
        with self.assertRaises(ValueError):
            output_identity(output, "manifest-b")
        with self.assertRaises(ValueError):
            aggregate_outputs(output, rows, "manifest-b")

    def fixture(self, transport):
        config = {"reader_model": model_config()["model"], "judge_model": model_config()["model"],
                  "reader_prompt_sha256": reader_prompt_sha256(),
                  "judge_prompt_sha256": judge_prompt_sha256(), "context_budget_tokens": 8000,
                  "qa": {"reader": model_config(), "judge": model_config(8)}}
        manifest = {"config_sha256": canonical_hash(config)}
        runner = QARunner(config, manifest, self.ledger, self.path / "cache", transport=transport,
                          packer=fake_pack, token_counter=lambda text, model: len(text))
        row = {"question_id": "synthetic", "question_type": "single-session-user",
               "question": "Where is the workshop?", "question_date": "2026-01-01",
               "answer": "GOLD_ONLY_SECRET", "haystack_sessions": [[{"has_answer": True}]]}
        artifact = {"schema_version": 1, "question_id": row["question_id"],
                    "manifest_sha256": runner.manifest_hash,
                    "records": [{"record_id": "actual", "content": "SANITIZED_ACTUAL_EVIDENCE"}]}
        path = self.path / "context.json"
        path.write_text(json.dumps(artifact))
        trace = {"question_id": row["question_id"], "manifest_sha256": runner.manifest_hash,
                 "status": "retrieval_complete", "context_sha256": canonical_hash(artifact),
                 "retrieved_ids": ["actual"]}
        return runner, row, path, trace

    def test_reader_gold_blind_judge_separate_and_full_resume(self):
        calls = []
        def fake(payload, timeout):
            calls.append(copy.deepcopy(payload))
            return response("workshop answer" if len(calls) == 1 else "yes")
        runner, row, path, trace = self.fixture(fake)
        result = runner.run_question(row, path, trace)
        self.assertEqual(result["status"], "completed")
        self.assertTrue(result["correct"])
        self.assertNotIn("GOLD_ONLY_SECRET", json.dumps(calls[0]))
        self.assertNotIn("has_answer", json.dumps(calls[0]))
        self.assertIn("SANITIZED_ACTUAL_EVIDENCE", json.dumps(calls[0]))
        self.assertIn("GOLD_ONLY_SECRET", json.dumps(calls[1]))
        self.assertEqual(result, runner.run_question(row, path, trace))
        self.assertEqual(len(calls), 2)

    def test_numeric_gold_reaches_judge_without_reader_leakage(self):
        calls = []
        def fake(payload, timeout):
            calls.append(copy.deepcopy(payload))
            return response("two" if len(calls) == 1 else "yes")
        runner, row, path, trace = self.fixture(fake)
        row["answer"] = 2
        result = runner.run_question(row, path, trace)
        self.assertEqual(result["status"], "completed")
        self.assertTrue(result["correct"])
        self.assertEqual(len(calls), 2)
        self.assertIn("Correct Answer: 2", calls[1]["messages"][0]["content"])
        self.assertNotIn("Correct Answer:", json.dumps(calls[0]))
        self.assertEqual(result, runner.run_question(row, path, trace))
        self.assertEqual(len(calls), 2)

    def test_unsupported_judge_inputs_fail_preflight_without_paid_reader(self):
        cases = [("answer", value) for value in [None, True, [], {}, float("nan"), float("inf")]]
        cases += [("question_type", "unknown"), ("question_type", []), ("question", None)]
        for field, value in cases:
            with self.subTest(field=field, value=value):
                calls = []
                def fake(payload, timeout):
                    calls.append(payload)
                    return response()
                runner, row, path, trace = self.fixture(fake)
                row[field] = value
                result = runner.run_question(row, path, trace)
                self.assertEqual(result["status"], "failed")
                self.assertEqual(result["failure_stage"], "judge_preflight")
                self.assertEqual(result["error_type"], "ValueError")
                self.assertNotIn("reader", result)
                self.assertNotIn("correct", result)
                self.assertEqual(calls, [])
                self.assertEqual(self.ledger.totals()["request_count"], 0)

    def test_missing_gold_fails_preflight_and_invalid_date_is_reader_failure(self):
        calls = []
        def fake(payload, timeout):
            calls.append(payload)
            return response()
        runner, row, path, trace = self.fixture(fake)
        del row["answer"]
        result = runner.run_question(row, path, trace)
        self.assertEqual(result["failure_stage"], "judge_preflight")
        self.assertEqual(result["error_type"], "KeyError")
        runner, row, path, trace = self.fixture(fake)
        row["question_date"] = None
        result = runner.run_question(row, path, trace)
        self.assertEqual(result["failure_stage"], "reader")
        self.assertEqual(result["error_type"], "ValueError")
        self.assertEqual(calls, [])
        self.assertEqual(self.ledger.totals()["request_count"], 0)

    def test_malformed_judge_is_failure_not_incorrect_label(self):
        calls = []
        def fake(payload, timeout):
            calls.append(payload)
            return response("reader" if len(calls) == 1 else "yes, probably")
        runner, row, path, trace = self.fixture(fake)
        result = runner.run_question(row, path, trace)
        self.assertEqual(result["failure_stage"], "judge")
        self.assertNotIn("correct", result)

    def test_artifact_mismatch_never_reaches_network(self):
        runner, row, path, trace = self.fixture(lambda *_: self.fail("no network"))
        path.write_text(path.read_text().replace("SANITIZED", "MUTATED"))
        result = runner.run_question(row, path, trace)
        self.assertEqual(result["failure_stage"], "retrieval")
        self.assertEqual(self.ledger.totals()["request_count"], 0)

    def test_http_transport_has_fixed_endpoint_and_no_retry(self):
        class FakeOpener:
            def __init__(self):
                self.requests = []
            def open(self, req, timeout):
                self.requests.append(req)
                raise TimeoutError()
        opener = FakeOpener()
        with patch.dict(os.environ, {"SYNTHETIC_TEST_KEY": "synthetic-credential"}):
            transport = OpenAITransport("SYNTHETIC_TEST_KEY", opener)
            with self.assertRaises(TimeoutError):
                transport({"model": model_config()["model"]}, 90)
        self.assertEqual(len(opener.requests), 1)
        self.assertEqual(opener.requests[0].full_url, "https://api.openai.com/v1/chat/completions")
        self.assertNotIn("synthetic-credential", opener.requests[0].data.decode())


if __name__ == "__main__":
    unittest.main()
