"""Occurrence-v1 synthetic fixtures; never ingest gold labels."""
import copy
import json
from pathlib import Path
import tempfile
from types import SimpleNamespace
import unittest
from representations import build_records, normalization_report, serialize_records
from runner import question_payload, retrieve, load_resume
from validate import canonical_hash
from qa_variants import neighbor_request_ids, variant_records
from evidence import evidence_coverage


class AdapterInputTests(unittest.TestCase):
    def question(self):
        return {"question_id": "synthetic", "question": "Which?", "answer": "GOLD",
                "haystack_session_ids": ["s"], "haystack_dates": ["2024-01-01"],
                "haystack_sessions": [[{"role": "user", "content": "First", "has_answer": True},
                                       {"role": "assistant", "content": "Second", "has_answer": False}]]}

    def duplicate(self, same_date=False):
        question = self.question()
        question["haystack_session_ids"].append("s")
        question["haystack_dates"].append("2024-01-01" if same_date else "2024-02-01")
        question["haystack_sessions"].append(copy.deepcopy(question["haystack_sessions"][0]))
        return question

    def raw(self, question):
        return build_records(question, "raw", input_normalization="occurrence-v1")

    def test_legacy_unique_schema_and_ids_unchanged(self):
        question = self.question()
        self.assertEqual(build_records(question, "raw"), self.raw(question))
        self.assertTrue(all(set(r["sources"][0]) == {"session_id", "turn_index"} for r in self.raw(question)))

    def test_repeated_ids_require_opt_in_and_preserve_dates(self):
        question = self.duplicate()
        with self.assertRaises(ValueError):
            build_records(question, "raw")
        records = self.raw(question)
        self.assertEqual([r["sources"][0]["session_occurrence"] for r in records], [0, 0, 1, 1])
        self.assertEqual([r["date"] for r in records], ["2024-01-01"] * 2 + ["2024-02-01"] * 2)
        self.assertEqual(len({r["record_id"] for r in records}), 4)
        self.assertNotIn("GOLD", serialize_records(records))
        self.assertNotIn("has_answer", serialize_records(records))

    def test_identical_date_and_text_still_keep_both_occurrences(self):
        records = self.raw(self.duplicate(same_date=True))
        self.assertEqual(len(records), 4)
        self.assertEqual(len({r["record_id"] for r in records}), 4)

    def test_different_text_shared_id_kept(self):
        question = self.duplicate()
        question["haystack_sessions"][1][0]["content"] = "Different"
        self.assertEqual(self.raw(question)[2]["content"], "Different")

    def test_exact_empty_skip_preserves_original_indices_and_ledger(self):
        question = self.question()
        question["haystack_sessions"][0].insert(1, {"role": "assistant", "content": ""})
        records = self.raw(question)
        self.assertEqual([r["sources"][0]["turn_index"] for r in records], [0, 2])
        self.assertEqual(neighbor_request_ids([records[0]["record_id"]], records), [])
        excluded = normalization_report(question)["excluded_turns"]
        self.assertEqual(len(excluded), 1)
        self.assertEqual(excluded[0]["source"], {"session_id": "s", "turn_index": 1})
        self.assertEqual(excluded[0]["reason"], "empty_content")
        with self.assertRaises(ValueError):
            build_records(question, "raw")

    def test_null_and_nonstring_are_not_skipped(self):
        for bad in [None, False, 0, []]:
            question = self.question()
            question["haystack_sessions"][0][0]["content"] = bad
            with self.subTest(bad=bad), self.assertRaises(ValueError):
                self.raw(question)

    def test_all_empty_question_still_fails_runner(self):
        question = self.question()
        for turn in question["haystack_sessions"][0]:
            turn["content"] = ""
        with self.assertRaises(ValueError):
            question_payload(question, {"mode": "raw", "input_normalization": "occurrence-v1"})

    def test_duplicate_fact_reference_requires_explicit_occurrence(self):
        question = self.duplicate()
        fact = {"content": "Known fact", "sources": [{"session_id": "s", "turn_index": 0}]}
        with self.assertRaises(ValueError):
            build_records(question, "facts", [fact], input_normalization="occurrence-v1")
        fact["sources"][0]["session_occurrence"] = 1
        records = build_records(question, "facts", [fact], input_normalization="occurrence-v1")
        self.assertEqual(records[0]["sources"][0]["session_occurrence"], 1)
        fact["sources"][0]["session_occurrence"] = True
        with self.assertRaises(ValueError):
            build_records(question, "facts", [fact], input_normalization="occurrence-v1")

    def test_neighbors_never_cross_occurrence_boundaries(self):
        records = self.raw(self.duplicate())
        self.assertEqual(neighbor_request_ids([records[0]["record_id"]], records), [records[1]["record_id"]])
        engine_sources = [{"record_id": r["record_id"], "content": r["content"], **r["sources"][0]} for r in records]
        result = variant_records("expanded60_neighbors", [engine_sources[2]], engine_sources)
        self.assertEqual([r["record_id"] for r in result], [records[2]["record_id"], records[3]["record_id"]])

    def test_evidence_coverage_does_not_alias_repeated_session_turns(self):
        question = self.duplicate()
        records = self.raw(question)
        coverage = evidence_coverage(question, records[:1])
        self.assertEqual(coverage["turn_provenance"]["recall"], 0.5)
        ambiguous = copy.deepcopy(records[:1])
        ambiguous[0]["sources"][0].pop("session_occurrence")
        self.assertFalse(evidence_coverage(question, ambiguous)["provenance_complete"])

    def normalized_retrieval(self, fail_readiness=False):
        question = self.question()
        question["haystack_sessions"][0].insert(1, {"role": "user", "content": ""})
        settings = {"mode": "raw", "top_k": 10, "input_normalization": "occurrence-v1"}
        ids = [item["record_id"] for item in question_payload(question, settings)[0]]
        def request(path, payload):
            if path == "/ingest":
                self.assertEqual([r["sources"][0]["turn_index"] for r in payload["records"]], [0, 2])
                return {"status": "complete", "accepted_ids": ids}
            if path == "/ready":
                return {"ready": not fail_readiness, "pending_jobs": 0,
                        "indexes_ready": True, "indexed_ids": ids}
            return {"retrieved_ids": ids}
        adapter = SimpleNamespace(settings=settings, identity=lambda: None,
                                  close=lambda: None, request=request)
        return question, settings, retrieve(adapter, question, "a" * 64)

    def test_normalization_ledger_automatically_bound_before_ingest(self):
        question, settings, result = self.normalized_retrieval()
        self.assertEqual(result["status"], "retrieval_complete")
        report = result["normalization_report"]
        self.assertEqual(report["manifest_sha256"], "a" * 64)
        self.assertEqual(report["excluded_turns"][0]["source"]["turn_index"], 1)
        self.assertEqual(result["normalization_sha256"], canonical_hash(report))
        self.assertNotIn("GOLD", json.dumps(report))
        self.assertNotIn("First", json.dumps(report))
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "trace.jsonl"
            path.write_text(json.dumps(result) + "\n")
            self.assertEqual(load_resume(path, [question], "a" * 64, settings), {question["question_id"]: result})

    def test_failed_retrieval_retains_required_normalization_ledger(self):
        question, settings, result = self.normalized_retrieval(fail_readiness=True)
        self.assertEqual(result["status"], "failed")
        self.assertEqual(result["failure_stage"], "readiness")
        self.assertIn("normalization_report", result)
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "trace.jsonl"
            path.write_text(json.dumps(result) + "\n")
            self.assertIn(question["question_id"], load_resume(path, [question], "a" * 64, settings))
            result.pop("normalization_report")
            result.pop("normalization_sha256")
            path.write_text(json.dumps(result) + "\n")
            with self.assertRaises(ValueError):
                load_resume(path, [question], "a" * 64, settings)

    def test_resume_rejects_missing_wrong_and_rehashed_tampered_ledger(self):
        question, settings, result = self.normalized_retrieval()
        missing = copy.deepcopy(result)
        missing.pop("normalization_report")
        wrong_hash = copy.deepcopy(result)
        wrong_hash["normalization_sha256"] = "b" * 64
        altered = copy.deepcopy(result)
        altered["normalization_report"]["excluded_turns"][0]["source"]["turn_index"] = 2
        altered["normalization_sha256"] = canonical_hash(altered["normalization_report"])
        wrong_mode = copy.deepcopy(result)
        wrong_mode["normalization_report"]["input_normalization"] = "legacy"
        wrong_mode["normalization_sha256"] = canonical_hash(wrong_mode["normalization_report"])
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "trace.jsonl"
            for bad in (missing, wrong_hash, altered, wrong_mode):
                with self.subTest(bad=bad["normalization_sha256"]):
                    path.write_text(json.dumps(bad) + "\n")
                    with self.assertRaises(ValueError):
                        load_resume(path, [question], "a" * 64, settings)

    def test_legacy_resume_rejects_unrequested_ledger(self):
        question = self.question()
        settings = {"mode": "raw", "top_k": 10}
        ids = [item["record_id"] for item in question_payload(question, settings)[0]]
        report = {**normalization_report(question), "manifest_sha256": "a" * 64}
        record = {"question_id": question["question_id"], "manifest_sha256": "a" * 64,
                  "status": "retrieval_complete", "ingested_ids": ids, "retrieved_ids": ids,
                  "evidence": {"coverage": "unavailable", "stages": []},
                  "normalization_report": report, "normalization_sha256": canonical_hash(report)}
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "trace.jsonl"
            path.write_text(json.dumps(record) + "\n")
            with self.assertRaises(ValueError):
                load_resume(path, [question], "a" * 64, settings)

    def test_malformed_source_failure_without_ledger_cannot_resume(self):
        question = self.question()
        question["haystack_sessions"][0][0]["content"] = None
        settings = {"mode": "raw", "top_k": 10, "input_normalization": "occurrence-v1"}
        def forbidden(*_):
            self.fail("Malformed source must fail before engine requests")
        adapter = SimpleNamespace(settings=settings, identity=forbidden,
                                  request=forbidden, close=lambda: None)
        result = retrieve(adapter, question, "a" * 64)
        self.assertEqual(result["failure_stage"], "prepare")
        self.assertNotIn("normalization_report", result)
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "trace.jsonl"
            path.write_text(json.dumps(result) + "\n")
            with self.assertRaises(ValueError):
                load_resume(path, [question], "a" * 64, settings)

    def test_unknown_normalization_rejected(self):
        with self.assertRaises(ValueError):
            build_records(self.question(), "raw", input_normalization="typo")


if __name__ == "__main__":
    unittest.main()
