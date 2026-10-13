import copy
import json
import unittest

from evidence import evidence_coverage
from representations import build_records, pack_records, serialize_records


def fixture():
    return {"question_id": "synthetic-pl-en", "answer": "DO_NOT_LEAK_GOLD",
            "answer_session_ids": ["may", "june"],
            "haystack_session_ids": ["may", "june"],
            "haystack_dates": ["2026-05-01", "2026-06-01"],
            "haystack_sessions": [
                [{"role": "assistant", "content": "I suggest blue.", "has_answer": False},
                 {"role": "user", "content": "Wolę pociąg, chyba że podróż trwa ponad 6 godzin.", "has_answer": True}],
                [{"role": "user", "content": "I previously chose blue. I now choose red, not green.", "has_answer": True}]]}


def fact():
    return {"content": "Użytkownik woli pociąg.",
            "sources": [{"session_id": "may", "turn_index": 1}]}


class RepresentationTests(unittest.TestCase):
    def test_label_blind_and_stable(self):
        question = fixture()
        raw = build_records(question, "raw")
        changed = copy.deepcopy(question)
        changed["answer"] = "other"
        changed["haystack_sessions"][0][0]["has_answer"] = True
        self.assertEqual(raw, build_records(changed, "raw"))
        self.assertNotIn("has_answer", serialize_records(raw))
        self.assertNotIn("DO_NOT_LEAK_GOLD", serialize_records(raw))
        self.assertEqual(raw[0]["role"], "assistant")
        self.assertEqual(raw[-1]["date"], "2026-06-01")
        other = copy.deepcopy(question)
        other["question_id"] = "other-corpus"
        self.assertNotEqual(raw[0]["record_id"], build_records(other, "raw")[0]["record_id"])

    def test_modes_and_unknown_legacy(self):
        raw = build_records(fixture(), "raw")
        facts = build_records(fixture(), "facts", [fact(), {"content": "legacy"}])
        self.assertEqual(build_records(fixture(), "dual", [fact()]), raw + facts[:1])
        self.assertEqual(facts[1]["sources"], [])
        self.assertIsNone(evidence_coverage(fixture(), facts)["turn_provenance"]["recall"])
        with self.assertRaises(ValueError):
            build_records(fixture(), "facts")
        invalid = fact()
        invalid["sources"][0]["turn_index"] = 99
        with self.assertRaises(ValueError):
            build_records(fixture(), "dual", [invalid])

    def test_no_first_sentence_loss_and_skip_large(self):
        raw = build_records(fixture(), "raw")
        huge = dict(raw[0], content="z" * 4000, record_id="huge")
        budget = len(serialize_records([raw[-1]]))
        # len is intentionally a synthetic character counter, NOT model tokens.
        packed = pack_records([huge, raw[-1]], budget, len)
        self.assertEqual(packed["records"], [raw[-1]])
        self.assertIn("not green", packed["context"])
        self.assertEqual(packed["token_count"], budget)
        self.assertEqual(packed["dropped_record_ids"], ["huge"])

    def test_dedup_requires_identical_content_and_origin(self):
        raw = build_records(fixture(), "raw")
        separate = dict(raw[0], record_id="separate", sources=raw[-1]["sources"])
        packed = pack_records([raw[0], raw[0], separate], 10000, len)
        self.assertEqual(packed["records"], [raw[0], separate])
        self.assertEqual(json.loads(packed["context"]), packed["records"])

    def test_budget_contract(self):
        for invalid in [-1, True, 1.2]:
            with self.assertRaises(ValueError):
                pack_records([], invalid, len)
        with self.assertRaises(ValueError):
            pack_records([], 0, len)
        with self.assertRaises(ValueError):
            pack_records([], 10, lambda text: 1.2)
        self.assertEqual(pack_records([], 2, len)["token_count"], 2)

    def test_all_support_not_any_support_and_fact_omission(self):
        raw = build_records(fixture(), "raw")
        full = evidence_coverage(fixture(), raw)
        self.assertTrue(full["full_source_turn"]["all_support_present"])
        partial = evidence_coverage(fixture(), raw[:2])
        self.assertEqual(partial["turn_provenance"]["recall"], 0.5)
        self.assertFalse(partial["session_provenance"]["all_support_present"])
        facts = build_records(fixture(), "facts", [fact()])
        observed = evidence_coverage(fixture(), facts)
        self.assertEqual(observed["full_source_turn"]["recall"], 0)
        self.assertEqual(observed["turn_provenance"]["recall"], 0.5)
        self.assertEqual(observed["semantic_support"], "not_measured")

    def test_truncated_source_does_not_count_as_full_evidence(self):
        raw = build_records(fixture(), "raw")
        raw[-1]["content"] = "I previously chose blue."
        observed = evidence_coverage(fixture(), raw)
        self.assertEqual(observed["turn_provenance"]["recall"], 1)
        self.assertEqual(observed["full_source_turn"]["recall"], 0.5)
        raw[-1]["sources"][0]["turn_index"] = 999
        self.assertIsNone(evidence_coverage(fixture(), raw)["turn_provenance"]["recall"])

    def test_missing_labels_remain_unknown(self):
        question = fixture()
        del question["haystack_sessions"][0][0]["has_answer"]
        del question["answer_session_ids"]
        observed = evidence_coverage(question, build_records(question, "raw"))
        self.assertIsNone(observed["turn_provenance"]["recall"])
        self.assertIsNone(observed["session_provenance"]["recall"])

    def test_ambiguous_session_alignment_rejected(self):
        question = fixture()
        question["haystack_session_ids"] = ["duplicate", "duplicate"]
        with self.assertRaises(ValueError):
            build_records(question, "raw")


if __name__ == "__main__":
    unittest.main()
