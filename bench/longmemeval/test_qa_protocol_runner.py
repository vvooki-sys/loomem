"""Fake-provider tests for the opt-in strict-v3-bounded runner."""

import copy
import json
from pathlib import Path
import tempfile
import unittest

from budget import BudgetLedger
from qa_prompts_strict_v3 import (
    PROTOCOL,
    judge_prompt_sha256,
    judge_response_format,
    reader_prompt_sha256,
    response_format_sha256,
)
from qa_protocol_runner import ProtocolQARunner, RunnerDependencies
from validate import canonical_hash


def model_config(max_tokens=512):
    return {
        "model": "gpt-4.1-2025-04-14",
        "context_window_tokens": 1047576,
        "max_completion_tokens": max_tokens,
        "input_usd_per_million": "2",
        "output_usd_per_million": "8",
        "temperature": 0,
        "seed": 42,
        "timeout_seconds": 90,
    }


def response(text, finish="stop"):
    return {
        "model": "gpt-4.1-2025-04-14",
        "choices": [{"message": {"content": text}, "finish_reason": finish}],
        "usage": {"prompt_tokens": 100, "completion_tokens": 8, "total_tokens": 108},
    }


def fake_pack(records, budget, model):
    return {
        "context": json.dumps(records),
        "context_tokens": 12,
        "selected_ids": [record["record_id"] for record in records],
        "empty": not records,
    }


class ProtocolRunnerTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.path = Path(self.tmp.name)
        self.ledger = BudgetLedger(self.path / "ledger.json", "100")

    def fixture(self, transport, *, reader_tokens=512):
        config = {
            "protocol": PROTOCOL,
            "reader_model": model_config()["model"],
            "judge_model": model_config()["model"],
            "reader_prompt_sha256": reader_prompt_sha256(),
            "judge_prompt_sha256": judge_prompt_sha256(),
            "judge_response_format_sha256": response_format_sha256(),
            "context_budget_tokens": 8000,
            "qa": {"reader": model_config(reader_tokens), "judge": model_config(64)},
        }
        manifest = {"config_sha256": canonical_hash(config)}
        runner = ProtocolQARunner(
            config,
            manifest,
            self.ledger,
            self.path / "cache",
            RunnerDependencies(
                transport=transport,
                packer=fake_pack,
                token_counter=lambda text, model: len(text),
            ),
        )
        row = {
            "question_id": "synthetic",
            "question_type": "single-session-user",
            "question": "Where is the workshop?",
            "question_date": "2026-01-01",
            "answer": "GOLD_ONLY_SECRET",
            "haystack_sessions": [[{"has_answer": True}]],
        }
        artifact = {
            "schema_version": 1,
            "question_id": row["question_id"],
            "manifest_sha256": runner.manifest_hash,
            "records": [{"record_id": "actual", "content": "SANITIZED_ACTUAL_EVIDENCE"}],
        }
        context_path = self.path / "context.json"
        context_path.write_text(json.dumps(artifact))
        trace = {
            "question_id": row["question_id"],
            "manifest_sha256": runner.manifest_hash,
            "status": "retrieval_complete",
            "context_sha256": canonical_hash(artifact),
            "retrieved_ids": ["actual"],
        }
        return runner, row, context_path, trace

    def test_fake_provider_gets_schema_only_for_judge_and_exact_label_completes(self):
        calls = []

        def fake(payload, timeout):
            calls.append(copy.deepcopy(payload))
            return response("workshop answer" if len(calls) == 1 else '{"label":"yes"}')

        runner, row, path, trace = self.fixture(fake)
        result = runner.run_question(row, path, trace)
        self.assertEqual(result["status"], "completed")
        self.assertTrue(result["correct"])
        self.assertEqual(len(calls), 2)
        self.assertNotIn("response_format", calls[0])
        self.assertEqual(calls[1]["response_format"], judge_response_format())
        self.assertNotIn("GOLD_ONLY_SECRET", json.dumps(calls[0]))
        self.assertNotIn("has_answer", json.dumps(calls[0]))
        self.assertIn("SANITIZED_ACTUAL_EVIDENCE", json.dumps(calls[0]))
        self.assertIn("GOLD_ONLY_SECRET", json.dumps(calls[1]))

    def test_gold_mutation_reuses_identical_reader_and_changes_only_judge_request(self):
        calls = []

        def fake(payload, timeout):
            calls.append(copy.deepcopy(payload))
            if "response_format" not in payload:
                return response("workshop answer")
            return response('{"label":"yes"}')

        runner, row, path, trace = self.fixture(fake)
        first = runner.run_question(row, path, trace)
        row["answer"] = "MUTATED_GOLD"
        second = runner.run_question(row, path, trace)
        self.assertEqual(first["status"], "completed")
        self.assertEqual(second["status"], "completed")
        self.assertEqual(len(calls), 3)
        self.assertNotIn("MUTATED_GOLD", json.dumps(calls[0]))
        self.assertNotIn("MUTATED_GOLD", json.dumps(calls[1]))
        self.assertIn("MUTATED_GOLD", json.dumps(calls[2]))
        self.assertEqual(first["reader"]["request_sha256"], second["reader"]["request_sha256"])
        self.assertNotEqual(first["judge"]["request_sha256"], second["judge"]["request_sha256"])

    def test_reader_non_stop_is_terminal_false_and_never_retried(self):
        calls = []

        def fake(payload, timeout):
            calls.append(copy.deepcopy(payload))
            return response("unfinished", finish="length")

        runner, row, path, trace = self.fixture(fake)
        first = runner.run_question(row, path, trace)
        second = runner.run_question(row, path, trace)
        self.assertEqual(first, second)
        self.assertEqual(first["status"], "failed")
        self.assertEqual(first["failure_stage"], "reader")
        self.assertEqual(first["reader"]["error_type"], "IncompleteCompletion")
        self.assertNotIn("correct", first)
        self.assertEqual(len(calls), 1)

    def test_malformed_judge_is_terminal_false_and_never_retried(self):
        calls = []
        malformed = [
            "Yes.",
            '{"label":"YES"}',
            '{"label":"yes","extra":1}',
            '{"label":"yes","label":"no"}',
            '{}',
            '[]',
            'null',
            '```json\n{"label":"yes"}\n```',
            '{"label":"yes"} because',
        ]
        current = {"text": malformed[0]}

        def fake(payload, timeout):
            calls.append(copy.deepcopy(payload))
            return response(current["text"] if "response_format" in payload else "answer")

        runner, row, path, trace = self.fixture(fake)
        for index, value in enumerate(malformed):
            with self.subTest(value=value):
                row["answer"] = f"GOLD_{index}"
                current["text"] = value
                first = runner.run_question(row, path, trace)
                second = runner.run_question(row, path, trace)
                self.assertEqual(first, second)
                self.assertEqual(first["status"], "failed")
                self.assertEqual(first["failure_stage"], "judge")
                self.assertEqual(first["error_type"], "ValueError")
                self.assertNotIn("correct", first)
        self.assertEqual(len(calls), 1 + len(malformed))

    def test_reader_ceiling_and_protocol_hashes_are_fail_closed(self):
        with self.assertRaisesRegex(ValueError, "512-token"):
            self.fixture(lambda *_: self.fail("network forbidden"), reader_tokens=511)

        runner, row, path, trace = self.fixture(lambda *_: self.fail("network forbidden"))
        config = copy.deepcopy(runner.config)
        config["judge_response_format_sha256"] = "0" * 64
        manifest = {"config_sha256": canonical_hash(config)}
        with self.assertRaisesRegex(ValueError, "protocol binding"):
            ProtocolQARunner(config, manifest, self.ledger, self.path / "other-cache")

    def test_schema_unavailable_stops_without_fallback_or_retry(self):
        calls = []

        def fake(payload, timeout):
            calls.append(copy.deepcopy(payload))
            if "response_format" in payload:
                raise ValueError("synthetic unsupported response format")
            return response("answer")

        runner, row, path, trace = self.fixture(fake)
        first = runner.run_question(row, path, trace)
        second = runner.run_question(row, path, trace)
        self.assertEqual(first, second)
        self.assertEqual(first["status"], "failed")
        self.assertEqual(first["failure_stage"], "judge")
        self.assertEqual(first["judge"]["error_type"], "ValueError")
        self.assertNotIn("correct", first)
        self.assertEqual(len(calls), 2)

    def test_judge_non_stop_is_terminal_false_and_never_parsed_or_retried(self):
        calls = []

        def fake(payload, timeout):
            calls.append(copy.deepcopy(payload))
            if "response_format" in payload:
                return response('{"label":"yes"}', finish="length")
            return response("answer")

        runner, row, path, trace = self.fixture(fake)
        first = runner.run_question(row, path, trace)
        second = runner.run_question(row, path, trace)
        self.assertEqual(first, second)
        self.assertEqual(first["status"], "failed")
        self.assertEqual(first["failure_stage"], "judge")
        self.assertEqual(first["judge"]["error_type"], "IncompleteCompletion")
        self.assertNotIn("correct", first)
        self.assertEqual(len(calls), 2)


if __name__ == "__main__":
    unittest.main()
