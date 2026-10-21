import copy
import json
from pathlib import Path
import tempfile
import unittest

import experiment as target


def question(qid, category="one", content=None):
    return {"question_id": qid, "question_type": category,
            "question": "Synthetic question", "answer": "Hidden label",
            "haystack_sessions": [[{"role": "user", "content": content or qid,
                                    "has_answer": True}]]}


def dataset():
    return [question(f"{category}-{i}" + ("_abs" if i < 2 else ""), category)
            for category in ("one", "two", "three") for i in range(8)]


class SplitTests(unittest.TestCase):
    def test_frozen_deterministic_balanced_and_disjoint(self):
        records = dataset()
        split = target.make_splits(records, 12, 6)
        self.assertEqual(split, target.make_splits(list(reversed(records)), 12, 6))
        self.assertEqual(len(split["dev"]), 12)
        self.assertEqual(len(split["eval"]), 12)
        self.assertEqual(len(split["pilot"]), 6)
        for key in ("question_id", "history_sha256"):
            self.assertFalse({row[key] for row in split["dev"]} & {row[key] for row in split["eval"]})
        self.assertTrue({row["question_id"] for row in split["pilot"]} <= {row["question_id"] for row in split["dev"]})
        for category in ("one", "two", "three"):
            rows = [row for row in split["pilot"] if row["question_type"] == category]
            self.assertEqual(len(rows), 2)
            self.assertTrue(any(row["abstention"] for row in rows))

    def test_shared_histories_stay_together_and_annotations_do_not_leak(self):
        records = dataset()
        duplicate = copy.deepcopy(records[0])
        duplicate.update(question_id="duplicate", question_type="two", answer="Changed label")
        duplicate["haystack_sessions"][0][0]["has_answer"] = False
        records.append(duplicate)
        self.assertEqual(target.history_hash(records[0]), target.history_hash(duplicate))
        split = target.make_splits(records, 12, 6)
        ids = {row["question_id"] for row in split["dev"]}
        self.assertEqual("duplicate" in ids, records[0]["question_id"] in ids)
        for row in split["dev"]:
            self.assertNotIn("answer", row)
            self.assertNotIn("content", row)

    def test_history_session_order_is_conservative_and_role_matters(self):
        original = question("q")
        original["haystack_sessions"].append([{"role": "assistant", "content": "next"}])
        changed = copy.deepcopy(original)
        changed["haystack_sessions"].reverse()
        self.assertEqual(target.history_hash(original), target.history_hash(changed))
        changed["haystack_sessions"][0][0]["role"] = "user"
        self.assertNotEqual(target.history_hash(original), target.history_hash(changed))

    def test_abstention_marker_matches_official_substring_rule(self):
        record = question("a_abs_variant")
        self.assertTrue(target.question_index([record])["a_abs_variant"]["abstention"])
        self.assertEqual(target.corpus_stats([record])["abstentions"], 1)

    def test_invalid_ids_and_impossible_group_sizes_fail(self):
        with self.assertRaises(ValueError):
            target.make_splits([question("a"), question("a")], 1, 1)
        records = [question(str(i), content="same history") for i in range(4)]
        with self.assertRaisesRegex(ValueError, "history group"):
            target.make_splits(records, 2, 1)
        with self.assertRaises(ValueError):
            target.make_splits(dataset(), 24, 6)

    def test_manifest_is_text_free_and_dataset_bound(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "data.json"
            path.write_text(json.dumps(dataset()))
            manifest = target.build_manifest(path, 12, 6)
            self.assertEqual(manifest["dataset_sha256"], target.sha256(path))
            encoded = json.dumps(manifest)
            self.assertNotIn("Synthetic question", encoded)
            self.assertNotIn("Hidden label", encoded)
            self.assertEqual(manifest["split_stats"]["pilot"]["turns"], 6)


class MetricTests(unittest.TestCase):
    def setUp(self):
        self.expected = list(target.question_index([
            question("a"), question("b_abs"), question("c", "two"), question("d", "two")
        ]).values())

    def test_denominator_includes_failed_and_missing_and_slices(self):
        metrics = target.qa_metrics(self.expected, {"a": True, "b_abs": False, "c": None})
        summary = metrics["summary"]
        self.assertEqual(summary["expected"], 4)
        self.assertEqual(summary["accuracy"], 0.25)
        self.assertEqual(summary["failed"], 1)
        self.assertEqual(summary["missing"], 1)
        self.assertEqual(metrics["categories"]["two"]["accuracy"], 0.0)
        self.assertEqual(metrics["abstentions"]["expected"], 1)
        self.assertTrue(summary["small_sample"])
        self.assertTrue(summary["wilson_95"][0] < 0.25 < summary["wilson_95"][1])

    def test_invalid_observations_rejected(self):
        for invalid in ({"unknown": True}, {"a": 1}, {"a": "true"}):
            with self.assertRaises(ValueError):
                target.qa_metrics(self.expected, invalid)

    def test_paired_delta_all_expected_and_deterministic(self):
        baseline = {"a": True, "b_abs": False, "c": None}
        candidate = {"a": False, "b_abs": True, "c": True}
        result = target.paired_metrics(self.expected, baseline, candidate)
        self.assertEqual(result, target.paired_metrics(self.expected, baseline, candidate))
        self.assertEqual(result["improved"], 2)
        self.assertEqual(result["regressed"], 1)
        self.assertEqual(result["unchanged"], 1)
        self.assertEqual(result["accuracy_delta"], 0.25)
        self.assertTrue(result["small_sample"])
        self.assertEqual(result["history_groups"], 4)

    def test_shared_history_bootstrap_and_empty_slice(self):
        for row in self.expected:
            row["history_sha256"] = "same"
        result = target.paired_metrics(self.expected, {}, {"a": True})
        self.assertEqual(result["history_groups"], 1)
        self.assertIsNone(result["paired_history_bootstrap_95"])
        self.assertIsNone(target.qa_metrics([], {})["summary"]["accuracy"])
        self.assertIsNone(target.wilson_interval(0, 0))
        low, high = target.wilson_interval(0, 18)
        self.assertAlmostEqual(low, 0)
        self.assertGreater(high, 0.17)


if __name__ == "__main__":
    unittest.main()
