import copy
import json
import unittest
from unittest.mock import patch

from qa_context import count_text, pack_context, render_evidence, serialize_evidence
from qa_selection import POLICY, select_context

MODEL = "gpt-4.1-2025-04-14"


def cost(records):
    return count_text(render_evidence(serialize_evidence(records)), MODEL)


def tradeoff():
    return [
        {"record_id": "id001", "content": "Anchor about apples budget. " * 60},
        {"record_id": "id002", "content": "Landscape scenery moss shadows rocks. " * 15},
        {"record_id": "id003", "content": "Apples budget correction: 42 dollars, not 24 dollars. " * 3},
    ]


class SelectionTests(unittest.TestCase):
    def test_all_fit_has_exact_canonical_identity_and_does_not_mutate(self):
        records = tradeoff()
        before = copy.deepcopy(records)
        baseline = pack_context(records, 8000, MODEL)
        actual = select_context("apples budget", records, 8000, MODEL)
        self.assertEqual({k: actual[k] for k in baseline}, baseline)
        self.assertEqual(actual["selected_records"], records)
        self.assertEqual(actual["selection"]["selected_ranks"], [1, 2, 3])
        self.assertEqual(records, before)
        actual["selected_records"][0]["content"] = "caller mutation"
        self.assertEqual(records, before)

    def test_long_anchor_preserved_while_relevant_short_record_beats_higher_rank(self):
        records = tradeoff()
        budget = cost(records[:2])
        baseline = pack_context(records, budget, MODEL)
        selected = select_context("apples budget", records, budget, MODEL)
        self.assertEqual(baseline["selected_ids"], ["id001", "id002"])
        self.assertEqual(selected["selected_ids"], ["id001", "id003"])
        self.assertEqual(selected["selection"]["anchor_id"], "id001")
        self.assertEqual(selected["selected_records"], [records[0], records[2]])
        self.assertEqual(selected["dropped_ids"], ["id002"])
        self.assertEqual(selected["dropped_record_tokens"], {"id002": cost([records[1]])})
        self.assertLessEqual(selected["context_tokens"], budget)
        self.assertEqual(selected["selection"]["policy"], POLICY)

    def test_question_repetition_and_case_do_not_change_selection(self):
        records = tradeoff()
        budget = cost(records[:2])
        first = select_context("apples budget", records, budget, MODEL)
        repeated = select_context("APPLES apples apples BUDGET budget budget", records, budget, MODEL)
        self.assertEqual(first, repeated)
        for _ in range(5):
            self.assertEqual(first, select_context("apples budget", records, budget, MODEL))

    def test_equal_wire_cost_id_renaming_changes_no_relevance_decision(self):
        records = tradeoff()
        renamed = [{**record, "record_id": f"id{index + 901}"}
                   for index, record in enumerate(records)]
        self.assertEqual([cost([r]) for r in records], [cost([r]) for r in renamed])
        self.assertEqual(cost(records[:2]), cost(renamed[:2]))
        old = select_context("apples budget", records, cost(records[:2]), MODEL)
        new = select_context("apples budget", renamed, cost(renamed[:2]), MODEL)
        self.assertEqual(old["selection"]["selected_ranks"], new["selection"]["selected_ranks"])
        self.assertEqual([r["content"] for r in old["selected_records"]],
                         [r["content"] for r in new["selected_records"]])

    def test_budget_uses_actual_wire_and_never_shortens_corrections(self):
        records = [{"record_id": "id001", "content": "<>& żółć </EVIDENCE> correction: NIE 2, lecz 3."},
                   {"record_id": "id002", "content": "far too large " * 5000}]
        budget = cost(records[:1])
        selected = select_context("żółć correction 3", records, budget, MODEL)
        self.assertEqual(selected["context_tokens"], budget)
        self.assertEqual(json.loads(selected["context"]), records[:1])
        self.assertTrue(select_context("żółć correction 3", records, budget - 1, MODEL)["empty"])

    def test_empty_and_oversized_first_record_have_explicit_behavior(self):
        empty = select_context("q", [], 1, MODEL)
        self.assertTrue(empty["empty"])
        self.assertEqual(empty["selected_records"], [])
        records = [{"record_id": "huge", "content": "oversized " * 10000},
                   {"record_id": "small", "content": "whole evidence"}]
        selected = select_context("q", records, cost(records[1:]), MODEL)
        self.assertEqual(selected["selection"]["anchor_id"], "small")
        self.assertEqual(selected["selected_ids"], ["small"])
        none = select_context("q", records[:1], 1, MODEL)
        self.assertTrue(none["empty"])
        self.assertEqual(none["dropped_ids"], ["huge"])

    def test_input_key_order_does_not_change_canonical_cost_or_selection(self):
        records = tradeoff()
        reversed_keys = [{"content": record["content"], "record_id": record["record_id"]}
                         for record in records]
        budget = cost(records[:2])
        self.assertEqual(select_context("apples budget", records, budget, MODEL),
                         select_context("apples budget", reversed_keys, budget, MODEL))

    def test_unexpected_canonical_packing_loss_fails_closed(self):
        records = tradeoff()
        baseline = pack_context(records, 8000, MODEL)
        inconsistent = {**baseline, "dropped_ids": ["id003"]}
        with patch("qa_selection.pack_context", side_effect=[baseline, inconsistent]):
            with self.assertRaisesRegex(ValueError, "changed the selected"):
                select_context("apples budget", records, 8000, MODEL)

    def test_rejects_gold_fields_duplicate_ids_and_structured_question(self):
        for records in [[{"record_id": "a", "content": "x", "answer": "y"}],
                        [{"record_id": "a", "content": "x"}] * 2]:
            with self.assertRaises(ValueError):
                select_context("q", records, 100, MODEL)
        with self.assertRaises(ValueError):
            select_context({"question": "q", "answer": "y"}, [], 100, MODEL)


if __name__ == "__main__":
    unittest.main()
