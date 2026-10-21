import io
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

import stage_diagnostics
from stage_diagnostics import aggregate, diagnose, markdown_report, question_diagnostics, stage_ids

MANIFEST = "a" * 64


def trace_row(question_id, pool, response, *, soft=None, filtered=None, rejections=None):
    stages = [("ranked_candidates", pool), ("after_soft_delete", soft if soft is not None else pool),
              ("after_filters_and_dedup", filtered if filtered is not None else pool),
              ("response", response)]
    row = {"question_id": question_id, "manifest_sha256": MANIFEST, "status": "retrieval_complete",
           "retrieved_ids": response,
           "evidence": {"coverage": "post_rank_to_response",
                        "stages": [{"stage": name, "chunk_ids": ids} for name, ids in stages]}}
    if rejections is not None:
        row["rejections"] = rejections
    return row


class DiagnosisPatternTests(unittest.TestCase):
    """The diagnosed failure: relevant ID survives to the pool, drops at response."""

    def setUp(self):
        pool = [f"c{i}" for i in range(1, 9)] + ["relevant-x", "c10"]
        self.row = trace_row("q1", pool, ["c1", "c2", "c3", "c4", "c5"],
                             filtered=pool[:-1],
                             rejections=[{"chunk_id": "c10", "stage": "after_filters_and_dedup",
                                          "reason": "dedup"},
                                         {"chunk_id": "relevant-x", "stage": "response",
                                          "reason": "top_k_cutoff"}])
        self.packed = {"selected_ids": ["c1", "c2", "c4"], "dropped_ids": ["c3", "c5"]}

    def test_relevant_present_at_pool_beyond_top_k_absent_in_response(self):
        report = question_diagnostics(self.row, ["relevant-x"], self.packed)
        entry = report["relevant"]["relevant-x"]
        self.assertEqual(entry["rank"], {"ranked_candidates": 9, "after_soft_delete": 9,
                                         "after_filters_and_dedup": 9, "response": None,
                                         "packed_context": None})
        self.assertEqual(entry["presence"], {"ranked_candidates": True, "after_soft_delete": True,
                                             "after_filters_and_dedup": True, "response": False,
                                             "packed_context": False})
        self.assertEqual(entry["first_absent"], "response")
        self.assertTrue(entry["pool_rank_beyond_top_k"])
        self.assertEqual(report["top_k"], 5)
        self.assertEqual(report["stage_counts"], {"ranked_candidates": 10, "after_soft_delete": 10,
                                                  "after_filters_and_dedup": 9, "response": 5,
                                                  "packed_context": 3})
        self.assertEqual(report["dropped_per_stage"], {"after_soft_delete": 0, "after_filters_and_dedup": 1,
                                                       "response": 4, "packed_context": 2})
        self.assertEqual(report["first_relevant_rank"]["ranked_candidates"], 9)
        self.assertIsNone(report["first_relevant_rank"]["response"])
        self.assertEqual(report["rejections_by_reason"], {"dedup": 1, "top_k_cutoff": 1})
        self.assertEqual(report["relevant_rejections"],
                         [{"chunk_id": "relevant-x", "stage": "response", "reason": "top_k_cutoff"}])

    def test_packed_rank_and_full_coverage(self):
        report = question_diagnostics(self.row, ["c4", "c2"], self.packed)
        self.assertEqual(report["relevant"]["c4"]["rank"]["packed_context"], 3)
        self.assertEqual(report["relevant"]["c4"]["rank"]["response"], 4)
        self.assertEqual(report["first_relevant_rank"],
                         {stage: 2 for stage in stage_diagnostics.STAGES})
        self.assertTrue(all(report["all_relevant_present"].values()))
        self.assertIsNone(report["relevant"]["c2"]["first_absent"])
        self.assertFalse(report["relevant"]["c2"]["pool_rank_beyond_top_k"])

    def test_aggregate_and_markdown(self):
        clean = trace_row("q2", ["r2", "o1"], ["r2"])
        report = diagnose([self.row, clean], {"q1": ["relevant-x"], "q2": ["r2"], "q9": ["z"]},
                          {"q1": self.packed})
        total = report["aggregate"]
        self.assertEqual(report["relevant_without_trace"], ["q9"])
        self.assertEqual(total["relevant_present"]["ranked_candidates"], {"present": 2, "of": 2})
        self.assertEqual(total["relevant_present"]["response"], {"present": 1, "of": 2})
        self.assertEqual(total["relevant_present"]["packed_context"], {"present": 0, "of": 1})
        self.assertEqual(total["questions_all_relevant_present"]["response"], {"count": 1, "of": 2})
        self.assertEqual(total["first_absent"]["response"], 1)
        self.assertEqual(total["never_absent"], 1)
        self.assertEqual(total["pool_rank_beyond_top_k"], 1)
        self.assertEqual(total["first_relevant_rank"]["ranked_candidates"],
                         {"n": 2, "min": 1, "median": 5, "max": 9})
        self.assertEqual(total["dropped_per_stage"]["response"], 5)
        self.assertIsNone(total["dropped_per_stage"]["ranked_candidates"])
        self.assertEqual(total["rejections_by_reason"], {"dedup": 1, "top_k_cutoff": 1})
        table = markdown_report(report)
        self.assertIn("| q1 | 1 | 1/1 (r9) | 1/1 (r9) | 1/1 (r9) | 0/1 | 0/1 | response |", table)
        self.assertIn("| q2 | 1 | 1/1 (r1) | 1/1 (r1) | 1/1 (r1) | 1/1 (r1) | n/a | - |", table)
        self.assertIn("| response | 1/2 | 1/2 | 1/1/1 | 5 |", table)
        self.assertIn("| top_k_cutoff | 1 |", table)


class UnmeasuredAndInvalidTests(unittest.TestCase):
    def test_unavailable_coverage_yields_null_not_false(self):
        row = {"question_id": "q", "status": "retrieval_complete", "retrieved_ids": ["a"],
               "evidence": {"coverage": "unavailable", "stages": []}}
        report = question_diagnostics(row, ["a"], None)
        self.assertTrue(all(value is None for value in report["relevant"]["a"]["presence"].values()))
        self.assertIsNone(report["relevant"]["a"]["first_absent"])
        self.assertIsNone(report["relevant"]["a"]["pool_rank_beyond_top_k"])
        self.assertTrue(all(value is None for value in report["stage_counts"].values()))
        total = aggregate([report])
        self.assertEqual(total["relevant_present"]["response"], {"present": 0, "of": 0})
        self.assertEqual(total["never_absent"], 1)

    def test_failed_row_and_no_relevant_ids(self):
        row = {"question_id": "q", "status": "failed", "failure_stage": "retrieval"}
        self.assertEqual(stage_ids(row), {stage: None for stage in stage_diagnostics.STAGES})
        report = question_diagnostics(row, [])
        self.assertEqual(report["relevant"], {})
        self.assertTrue(all(value is None for value in report["all_relevant_present"].values()))

    def test_invalid_inputs_fail_closed(self):
        row = trace_row("q", ["a"], ["a"])
        for relevant in [["a", "a"], [1], "a"]:
            with self.subTest(relevant=relevant), self.assertRaises(ValueError):
                question_diagnostics(row, relevant)
        with self.assertRaises(ValueError):
            question_diagnostics(row, ["a"], {"selected_ids": "a", "dropped_ids": []})
        with self.assertRaises(ValueError):
            question_diagnostics(row, ["a"], {"selected_ids": ["a"]})
        with self.assertRaises(ValueError):
            question_diagnostics(dict(row, rejections=[{"chunk_id": "a", "reason": "x"}]), ["a"])
        bad = trace_row("q", ["a"], ["a"])
        bad["evidence"]["stages"][0]["stage"] = "mystery"
        with self.assertRaises(ValueError):
            question_diagnostics(bad, ["a"])
        with self.assertRaises(ValueError):
            diagnose([row, row], {})
        with self.assertRaises(ValueError):
            diagnose([row], [])

    def test_rejections_inside_evidence_are_also_read(self):
        row = trace_row("q", ["a", "b"], ["a"])
        row["evidence"]["rejections"] = [{"chunk_id": "b", "stage": "response", "reason": "top_k_cutoff"}]
        report = question_diagnostics(row, ["b"])
        self.assertEqual(report["rejections_by_reason"], {"top_k_cutoff": 1})
        self.assertEqual(len(report["relevant_rejections"]), 1)


class CliTests(unittest.TestCase):
    def test_cli_reads_files_and_writes_reports(self):
        with tempfile.TemporaryDirectory() as directory:
            base = Path(directory)
            trace = base / "trace.jsonl"
            trace.write_text(json.dumps(trace_row("q1", ["x", "rel"], ["x"])) + "\n")
            (base / "relevant.json").write_text(json.dumps({"q1": ["rel"]}))
            (base / "packed.json").write_text(json.dumps({"q1": {"selected_ids": ["x"], "dropped_ids": []}}))
            out, md = base / "report.json", base / "report.md"
            with patch("sys.stdout", new_callable=io.StringIO) as stdout:
                code = stage_diagnostics.main(["--trace", str(trace), "--relevant", str(base / "relevant.json"),
                                               "--packed", str(base / "packed.json"), "--output", str(out),
                                               "--markdown", str(md), "--format", "markdown"])
            self.assertEqual(code, 0)
            report = json.loads(out.read_text())
            self.assertEqual(report["questions"][0]["relevant"]["rel"]["first_absent"], "response")
            self.assertEqual(stdout.getvalue(), md.read_text())
            self.assertIn("| q1 | 1 | 1/1 (r2) |", md.read_text())
            (base / "dup.json").write_text('{"q1": ["a"], "q1": ["b"]}')
            with patch("sys.stderr", new_callable=io.StringIO):
                self.assertEqual(stage_diagnostics.main(["--trace", str(trace), "--relevant",
                                                         str(base / "dup.json")]), 2)


if __name__ == "__main__":
    unittest.main()
