"""Independent runner boundary checks. Synthetic in-process adapter; no network."""
import argparse
import copy
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

import runner
from representations import build_records
from validate import build_manifest


def question():
    return {"question_id": "synthetic_contract", "question_type": "single-session-user",
            "question": "What changed?", "answer": "SECRET_GOLD",
            "question_date": "2026/10/01 (Thu) 08:15",
            "answer_session_ids": ["session"], "haystack_session_ids": ["session"],
            "haystack_dates": ["2026-10-01"], "haystack_sessions": [[
                {"role": "user", "content": "I chose blue. Now I choose red.",
                 "has_answer": True}]]}


class SyntheticAdapter:
    def __init__(self, behavior):
        self.settings = {"top_k": 5, "mode": "raw"}
        self.behavior = behavior
        self.calls = []
        self.closed = False

    def identity(self):
        pass

    def close(self):
        self.closed = True

    def request(self, path, payload):
        self.calls.append((path, payload))
        if path == "/ingest":
            self.ids = [record["record_id"] for record in payload["records"]]
            return {"status": "complete", "accepted_ids": self.ids}
        if path == "/ready":
            return {"ready": True, "pending_jobs": 0, "indexes_ready": True,
                    "indexed_ids": self.ids}
        stage_names = ["ranked_candidates", "after_soft_delete", "after_filters_and_dedup", "response"]
        stages = [{"stage": name, "chunk_ids": self.ids} for name in stage_names]
        if self.behavior == "foreign_trace":
            stages[0]["chunk_ids"] = ["foreign_question_chunk"]
        if self.behavior == "response_mismatch":
            stages[-1]["chunk_ids"] = []
        return {"retrieved_ids": self.ids, "trace_metadata": {"evidence": {
            "schema_version": 1, "coverage": "post_rank_to_response", "stages": stages}}}


class RunnerContractTests(unittest.TestCase):
    def test_adapter_never_receives_gold_and_always_closes(self):
        adapter = SyntheticAdapter("ok")
        result = runner.retrieve(adapter, question(), "f" * 64)
        self.assertEqual(result["status"], "retrieval_complete")
        wire = json.dumps(adapter.calls)
        for forbidden in ("SECRET_GOLD", "has_answer", "answer_session_ids", "synthetic_contract"):
            self.assertNotIn(forbidden, wire)
        self.assertTrue(adapter.closed)

    def test_foreign_trace_is_rejected(self):
        adapter = SyntheticAdapter("foreign_trace")
        result = runner.retrieve(adapter, question(), "f" * 64)
        self.assertEqual(result["status"], "failed")
        self.assertEqual(result["failure_stage"], "retrieval")
        self.assertTrue(adapter.closed)

    def test_response_trace_must_match_retrieved_ids(self):
        result = runner.retrieve(SyntheticAdapter("response_mismatch"), question(), "f" * 64)
        self.assertEqual(result["status"], "failed")
        self.assertEqual(result["failure_stage"], "retrieval")

    def test_gold_edits_do_not_change_ingest_projection(self):
        changed = copy.deepcopy(question())
        changed["answer"] = "different"
        changed["answer_session_ids"] = []
        changed["haystack_sessions"][0][0]["has_answer"] = False
        self.assertEqual(runner.question_payload(question()), runner.question_payload(changed))

    def test_gold_and_id_mutation_do_not_change_e1_wire_plan(self):
        settings = {"operation_policy": "E1-C"}
        original = question()
        changed = copy.deepcopy(original)
        changed.update(question_id="other", question_type="temporal-reasoning",
                       answer="changed", answer_session_ids=[])
        changed["haystack_sessions"][0][0]["has_answer"] = False
        self.assertEqual(runner.operation_fields(original, settings),
                         runner.operation_fields(changed, settings))

    def test_question_clock_mutation_is_bound_only_for_date_cells(self):
        changed = copy.deepcopy(question())
        changed["question_date"] = "2026/10/02 (Fri) 08:15"
        for policy in ("E0", "E1-F"):
            self.assertEqual(runner.operation_fields(question(), {"operation_policy": policy}),
                             runner.operation_fields(changed, {"operation_policy": policy}))
        for policy in ("E1-D", "E1-C"):
            self.assertNotEqual(runner.operation_fields(question(), {"operation_policy": policy}),
                                runner.operation_fields(changed, {"operation_policy": policy}))

    def test_e2_wire_clock_and_debug_are_explicit(self):
        for policy in sorted(runner.E2_POLICIES):
            for debug in (False, True):
                with self.subTest(policy=policy, debug=debug):
                    adapter = SyntheticAdapter("ok")
                    adapter.settings.update(operation_policy=policy, debug_operation=debug)
                    result = runner.retrieve(adapter, question(), "f" * 64)
                    self.assertEqual(result["status"], "retrieval_complete")
                    wire = next(body for path, body in adapter.calls if path == "/search")
                    expected = {"scope", "query", "top_k", "trace", "operation_policy"}
                    if debug:
                        expected.add("debug_operation")
                        self.assertIs(wire["debug_operation"], True)
                    if policy != "E2-F":
                        expected.add("reference_time")
                        self.assertEqual(wire["reference_time"], "2026-10-01T08:15:00Z")
                    self.assertEqual(set(wire), expected)
                    self.assertNotIn("reference_time_sha256", result)
                    self.assertNotIn("2026-10-01T08:15:00Z", json.dumps(result))
                    self.assertTrue(adapter.closed)

    def test_e2_invalid_or_absent_clock_never_gets_fallback(self):
        for value in (None, "", "2026/10/01 (Fri) 08:15", "2026/02/30 (Mon) 08:15",
                      "2026/10/01 (Thu) 25:00", "2026-10-01T08:15:00Z"):
            for policy in ("E2-D", "E2-C"):
                with self.subTest(value=value, policy=policy):
                    row = {**question(), "question_date": value}
                    wire, trace = runner.operation_fields(row, {"operation_policy": policy})
                    self.assertEqual(wire, {"operation_policy": policy})
                    self.assertEqual(trace, {"operation_policy": policy})

    def test_e2_gold_id_category_mutation_cannot_change_wire(self):
        changed = copy.deepcopy(question())
        changed.update(question_id="other", question_type="temporal-reasoning",
                       answer="changed", answer_session_ids=[])
        changed["haystack_sessions"][0][0]["has_answer"] = False
        for policy in sorted(runner.E2_POLICIES):
            settings = {"operation_policy": policy}
            self.assertEqual(runner.operation_fields(question(), settings),
                             runner.operation_fields(changed, settings))

    def test_e2_clock_changes_wire_but_never_public_trace(self):
        changed = {**question(), "question_date": "2026/10/02 (Fri) 08:15"}
        for policy in sorted(runner.E2_POLICIES):
            before = runner.operation_fields(question(), {"operation_policy": policy})
            after = runner.operation_fields(changed, {"operation_policy": policy})
            self.assertEqual(before[1], after[1])
            self.assertEqual(before[0] == after[0], policy == "E2-F")

    def test_e2_identity_rejects_policy_switch_and_legacy_anchor_hash(self):
        for policy in sorted(runner.E2_POLICIES):
            settings = {"operation_policy": policy}
            runner.verify_operation({"operation_policy": policy}, question(), settings)
            for changed in ({"operation_policy": "E0"},
                            {"operation_policy": policy, "reference_time_sha256": "a" * 64}):
                with self.subTest(policy=policy, changed=changed), self.assertRaises(ValueError):
                    runner.verify_operation(changed, question(), settings)

    def test_dataset_digest_change_rejected_before_adapter(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            args = argparse.Namespace(dataset=root / "data.json", config=root / "config.json",
                manifest=root / "manifest.json", trace=root / "trace.jsonl", dry_run=False, resume=False)
            config = {key: "a" * size for key, size in [
                ("engine_sha", 40), ("harness_sha", 40), ("reader_prompt_sha256", 64),
                ("judge_prompt_sha256", 64), ("execution_config_sha256", 64)]}
            config.update(reader_model="disabled", judge_model="disabled", embedding_model="synthetic",
                          seed=0, context_budget_tokens=2000)
            args.config.write_text(json.dumps(config))
            args.dataset.write_text(json.dumps([question()]))
            args.manifest.write_text(json.dumps(build_manifest(args.dataset, args.config)))
            changed = question()
            changed["answer"] = "changed after freeze"
            args.dataset.write_text(json.dumps([changed]))
            with patch.object(runner, "Adapter", side_effect=AssertionError("must not connect")):
                with self.assertRaises(ValueError):
                    runner.run(args)
            self.assertFalse(args.trace.exists())

    def test_failed_resume_cannot_smuggle_unvalidated_evidence(self):
        row = question()
        record = {"question_id": row["question_id"], "manifest_sha256": "f" * 64,
                  "status": "failed", "failure_stage": "retrieval",
                  "evidence": {"answer": "UNVALIDATED_TEXT"}}
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "trace.jsonl"
            path.write_text(json.dumps(record) + "\n")
            with self.assertRaises(ValueError):
                runner.load_resume(path, [row], "f" * 64, {"top_k": 5, "mode": "raw"})

    def test_resume_rejects_foreign_stage_id(self):
        row = question()
        ids = [record["record_id"] for record in build_records(row, "raw")]
        stages = [{"stage": name, "chunk_ids": ids} for name in
                  ("ranked_candidates", "after_soft_delete", "after_filters_and_dedup", "response")]
        stages[0]["chunk_ids"] = ["foreign_question_chunk"]
        record = {"question_id": row["question_id"], "manifest_sha256": "f" * 64,
                  "status": "retrieval_complete", "ingested_ids": ids, "retrieved_ids": ids,
                  "evidence": {"coverage": "post_rank_to_response", "stages": stages}}
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "trace.jsonl"
            path.write_text(json.dumps(record) + "\n")
            with self.assertRaises(ValueError):
                runner.load_resume(path, [row], "f" * 64, {"top_k": 5, "mode": "raw"})


if __name__ == "__main__":
    unittest.main()
