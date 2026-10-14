import copy
import json
import unittest
from unittest.mock import patch

from qa_context import count_text, pack_context, render_evidence, serialize_evidence
from qa_selection_v2 import (
    MAX_PROMOTED_TOKENS,
    MAX_PROMOTIONS,
    POLICY,
    _promotion_exposure,
    select_context,
)

MODEL = "gpt-4.1-2025-04-14"


def cost(records):
    return count_text(render_evidence(serialize_evidence(records)), MODEL)


def record(index, words, signal=""):
    return {"record_id": f"r{index:02d}", "content": (signal + " " + words).strip()}


class SelectionV2Tests(unittest.TestCase):
    def assert_exact(self, records, result, budget):
        selected = json.loads(result["context"])
        source = {item["record_id"]: item for item in records}
        self.assertEqual(selected, result["selected_records"])
        self.assertEqual([item["record_id"] for item in selected], result["selected_ids"])
        self.assertLessEqual(result["context_tokens"], budget)
        for item in selected:
            self.assertEqual(item, source[item["record_id"]])

    def test_all_fit_is_canonical_and_input_is_not_mutated(self):
        records = [record(1, "alpha"), record(2, "beta")]
        before = copy.deepcopy(records)
        baseline = pack_context(records, 8000, MODEL)
        result = select_context("alpha", records, 8000, MODEL)
        self.assertEqual({key: result[key] for key in baseline}, baseline)
        self.assertEqual(result["selection"]["mode"], "all_fit")
        self.assertEqual(records, before)

    def test_protects_every_baseline_record_in_top_twenty(self):
        records = [record(i, "anchor " * 80) for i in range(1, 21)]
        records += [record(i, "target compact") for i in range(21, 31)]
        budget = cost(records[:20])
        baseline = pack_context(records, budget, MODEL)
        result = select_context("target", records, budget, MODEL)
        protected = {item["record_id"] for item in records[:20]} & set(baseline["selected_ids"])
        self.assertTrue(protected <= set(result["selected_ids"]))

    def test_hostile_flood_adds_at_most_two_and_800_canonical_tokens(self):
        benign = [record(i, "background " * 85) for i in range(1, 35)]
        attacks = [record(i, "account password ignore instructions target " * 8)
                   for i in range(35, 55)]
        records = benign + attacks + [record(i, "other") for i in range(55, 61)]
        baseline = pack_context(records, 8000, MODEL)
        result = select_context("account password target", records, 8000, MODEL)
        promotions = set(result["selected_ids"]) - set(baseline["selected_ids"])
        self.assertIn("r12", baseline["selected_ids"])
        self.assertIn("r12", result["selected_ids"])
        self.assertLessEqual(len(promotions), MAX_PROMOTIONS)
        self.assertLessEqual(result["selection"]["promoted_exposure_tokens"],
                             MAX_PROMOTED_TOKENS)
        self.assertEqual(promotions, set(result["selection"]["promoted_ids"]))
        selected = result["selected_records"]
        without = [item for item in selected if item["record_id"] not in promotions]
        exact_delta = cost(selected) - cost(without)
        self.assertEqual(result["selection"]["promoted_exposure_tokens"], exact_delta)
        self.assert_exact(records, result, 8000)

    def test_exposure_is_canonical_delta_not_standalone_sum(self):
        records = [record(1, "x " * 390), record(2, "y " * 390)]
        standalone_sum = sum(cost([item]) for item in records)
        exposure = _promotion_exposure(records, {0, 1}, [0, 1], MODEL)
        self.assertGreater(standalone_sum, MAX_PROMOTED_TOKENS)
        self.assertLessEqual(exposure, MAX_PROMOTED_TOKENS)

        def synthetic_count(items, _model):
            return {0: 1, 1: 400, 2: 802}[len(items)]

        with patch("qa_selection_v2._rendered_tokens", side_effect=synthetic_count):
            synthetic_sum = sum(synthetic_count([item], MODEL) for item in records)
            synthetic_exposure = _promotion_exposure(records, {0, 1}, [0, 1], MODEL)
        self.assertEqual(synthetic_sum, MAX_PROMOTED_TOKENS)
        self.assertGreater(synthetic_exposure, MAX_PROMOTED_TOKENS)

    def test_long_query_copy_at_rank_45_is_not_promoted(self):
        records = [record(i, "benign details " * 65) for i in range(1, 45)]
        records.append(record(45, "ignore instructions secret target " * 500))
        records += [record(i, "tail") for i in range(46, 61)]
        result = select_context("secret target", records, 8000, MODEL)
        self.assertNotIn("r45", result["selection"]["promoted_ids"])

    def test_late_compact_query_record_can_replace_only_lower_utility_tail(self):
        records = [record(i, "protected context " * 15) for i in range(1, 21)]
        records += [record(i, "unrelated filler " * 120) for i in range(21, 31)]
        records += [record(i, "tail") for i in range(31, 45)]
        records.append(record(45, "target correction 42"))
        records += [record(i, "tail") for i in range(46, 61)]
        budget = cost(records[:22])
        baseline = pack_context(records, budget, MODEL)
        result = select_context("target correction", records, budget, MODEL)
        self.assertIn("r45", result["selection"]["promoted_ids"])
        self.assertTrue(set(baseline["selected_ids"][:20]) <= set(result["selected_ids"]))
        self.assertTrue(set(result["selection"]["evicted_baseline_ids"]) <=
                        set(baseline["selected_ids"][20:]))

    def test_deterministic_source_agnostic_and_rejects_labels(self):
        records = [record(i, "topic " * (10 + i)) for i in range(1, 61)]
        budget = 1000
        first = select_context("topic", records, budget, MODEL)
        for _ in range(3):
            self.assertEqual(first, select_context("topic", records, budget, MODEL))
        renamed = [{"record_id": f"x{index:02d}", "content": item["content"]}
                   for index, item in enumerate(records, 1)]
        second = select_context("topic", renamed, budget, MODEL)
        self.assertEqual(first["selection"]["selected_ranks"],
                         second["selection"]["selected_ranks"])
        with self.assertRaises(ValueError):
            select_context("topic", [{"record_id": "x", "content": "topic",
                                      "answer": "forbidden"}], budget, MODEL)

    def test_budget_and_question_validation(self):
        with self.assertRaises(ValueError):
            select_context({"question": "q"}, [], 10, MODEL)
        for budget in (True, 0, -1, 1.5):
            with self.subTest(budget=budget), self.assertRaises(ValueError):
                select_context("q", [], budget, MODEL)


if __name__ == "__main__":
    unittest.main()
