"""Offline plan §8C review-list sampler: allocation, determinism, mandatory pairs, create-only output."""
import hashlib
import io
import json
from pathlib import Path
import sys
import tempfile
from types import SimpleNamespace
import unittest
from unittest import mock

from paid_test_support import write_holdout
from review_sample import SampleError, allocate, build_list, main

F, S, A = "facts_retry", "selective_originals", "always_originals"
# The r5 strata from the 8C cost document (category: agree / disagree).
R5 = {"cross_session_dependency": (46, 4), "temporal_update": (37, 3), "qualifiers_preferences": (37, 3),
      "unanswerable": (25, 0), "conflict_entity_assignment": (22, 3), "list_count": (13, 7)}


def verdict(qid, arm, score="correct", candidate="An answer."):
    return {"question_id": qid, "arm": arm, "candidate": candidate, "score": score, "request_id": "r",
            "verdict": None, "usage": None, "outcome": "ok"}


def synthetic(spec=R5):
    """A holdout stub and F/S verdicts whose agreement strata match ``spec``."""
    order, questions, verdicts = [], {}, {}
    for category, (agree, disagree) in spec.items():
        for i in range(agree + disagree):
            qid = f"{category[:4]}-{i:03d}"
            order.append(qid)
            questions[qid] = {"category": category}
            verdicts[(qid, F)] = verdict(qid, F)
            verdicts[(qid, S)] = verdict(qid, S, "correct" if i < agree else "incorrect")
    return SimpleNamespace(question_order=tuple(order), questions=questions, manifest_sha256="0" * 64), verdicts


class AllocationTests(unittest.TestCase):
    def test_r5_allocation_matches_the_cost_document(self):
        holdout, verdicts = synthetic()
        doc = build_list(verdicts, holdout, "seed", 100, [F])
        self.assertEqual({c: a["agree"] + a["disagree"] for c, a in doc["allocation"].items()},
                         {"cross_session_dependency": 25, "temporal_update": 20, "qualifiers_preferences": 20,
                          "unanswerable": 12, "conflict_entity_assignment": 13, "list_count": 10})
        self.assertEqual({c: a["disagree"] for c, a in doc["allocation"].items()},
                         {"cross_session_dependency": 2, "temporal_update": 2, "qualifiers_preferences": 2,
                          "unanswerable": 0, "conflict_entity_assignment": 2, "list_count": 4})
        self.assertEqual(len(doc["sampled_question_ids"]), 100)
        self.assertEqual(doc["strata"]["list_count"], {"agree": 13, "disagree": 7})

    def test_at_least_one_disagreement_when_the_stratum_exists(self):
        strata = {"x": {"agree": [f"a{i}" for i in range(99)], "disagree": ["d0"]},
                  "y": {"agree": [f"b{i}" for i in range(100)], "disagree": []}}
        self.assertEqual(allocate(strata, 10), {"x": {"agree": 4, "disagree": 1}, "y": {"agree": 5, "disagree": 0}})
        with self.assertRaises(SampleError):
            allocate(strata, 201)

    def test_a_disagreement_category_with_a_zero_share_gets_a_reserved_place(self):
        strata = {"x": {"agree": [f"a{i}" for i in range(99)], "disagree": []},
                  "y": {"agree": [], "disagree": ["d0"]}}  # proportional share of y: 10 x 1/100 -> 0
        self.assertEqual(allocate(strata, 10), {"x": {"agree": 9, "disagree": 0}, "y": {"agree": 0, "disagree": 1}})
        chain = {"x": {"agree": [f"a{i}" for i in range(97)], "disagree": []},
                 **{c: {"agree": [], "disagree": [f"{c}0"]} for c in ("p", "q", "r")}}
        self.assertEqual(allocate(chain, 3), {"p": {"agree": 0, "disagree": 1}, "q": {"agree": 0, "disagree": 1},
                                              "r": {"agree": 0, "disagree": 1}, "x": {"agree": 0, "disagree": 0}})
        with self.assertRaises(SampleError):  # 3 categories need a disagreement place, 2 places
            allocate(chain, 2)


class ListTests(unittest.TestCase):
    def test_deterministic_and_seeded(self):
        holdout, verdicts = synthetic()
        one = build_list(verdicts, holdout, "seed-a", 100, [F, S])
        self.assertEqual(one, build_list(verdicts, holdout, "seed-a", 100, [F, S]))
        self.assertNotEqual(one["sampled_question_ids"],
                            build_list(verdicts, holdout, "other", 100, [F, S])["sampled_question_ids"])
        sampled = set(one["sampled_question_ids"])
        listed = {(p["question_id"], p["arm"]) for p in one["pairs"]}
        self.assertTrue({(q, arm) for q in sampled for arm in (F, S)} <= listed)
        position = {q: i for i, q in enumerate(holdout.question_order)}
        keys = [(position[p["question_id"]], [F, S, A].index(p["arm"])) for p in one["pairs"]]
        self.assertEqual(keys, sorted(keys))
        self.assertEqual(len(keys), len(set(keys)))

    def test_regressions_and_critical_answers_are_always_added(self):
        spec = {"list_count": (30, 0), "temporal_update": (10, 0)}
        holdout, verdicts = synthetic(spec)
        verdicts[("list-005", S)] = verdict("list-005", S, "incorrect")        # F/S regression
        verdicts[("list-007", A)] = verdict("list-007", A, "invalid_judge")    # F/A regression
        verdicts[("list-007", S)] = verdict("list-007", S, "technical_error", candidate=None)  # unreviewable
        verdicts[("list-009", S)] = verdict("list-009", S, "technical_error", candidate=None)  # F/S, S unreviewable
        verdicts[("temp-003", F)] = verdict("temp-003", F, "incorrect")        # critical category, not correct
        doc = build_list(verdicts, holdout, "s", 2, [F])  # 2 categories with a disagreement: 1 place each
        listed = {(p["question_id"], p["arm"]) for p in doc["pairs"]}
        for pair in [("list-005", F), ("list-005", S), ("list-007", F), ("list-007", A), ("list-009", F),
                     ("temp-003", F)]:
            self.assertIn(pair, listed)
        self.assertEqual(doc["mandatory"]["regressions_f_s"], ["list-005", "list-007", "list-009"])
        self.assertEqual(doc["mandatory"]["regressions_f_a"], ["list-007"])
        self.assertIn(["temp-003", F], doc["mandatory"]["critical_not_correct"])
        self.assertEqual({(e["question_id"], e["arm"]) for e in doc["excluded_unreviewable"]},
                         {("list-007", S), ("list-009", S)})
        self.assertNotIn(("list-009", S), listed)

    def test_included_pairs_and_generator_metadata(self):
        holdout, verdicts = synthetic({"list_count": (30, 0)})
        doc = build_list(verdicts, holdout, "s", 1, [F], include_pairs=[("list-020", S), ("list-020", S)])
        self.assertIn({"question_id": "list-020", "arm": S}, doc["pairs"])
        self.assertEqual(doc["mandatory"]["included_pairs"], [["list-020", S]])
        self.assertEqual(doc["python"], sys.version)
        self.assertIn("random.Random", doc["random"])
        for bad in ([("list-020", A)], [("nope-001", F)]):  # no verdict for the pair / not a holdout question
            with self.subTest(bad=bad), self.assertRaises(SampleError):
                build_list(verdicts, holdout, "s", 1, [F], include_pairs=bad)


    def test_a_sampled_pair_without_a_verdict_is_refused(self):
        holdout, verdicts = synthetic()
        complete = build_list(verdicts, holdout, "seed", 100, [F, S])
        qid = complete["sampled_question_ids"][7]
        del verdicts[(qid, S)]  # e.g. an interrupted judge run
        with self.assertRaises(SampleError) as caught:
            build_list(verdicts, holdout, "seed", 100, [F, S])
        self.assertIn(repr((qid, S)), str(caught.exception))

    def test_always_originals_is_expected_only_on_the_always_list(self):
        holdout, verdicts = synthetic({"list_count": (4, 0)})
        holdout.always_ids = frozenset({"list-000"})
        verdicts[("list-000", A)] = verdict("list-000", A)
        doc = build_list(verdicts, holdout, "s", 4, [F, A])  # A never runs on list-001..003: not missing
        self.assertEqual([(p["question_id"], p["arm"]) for p in doc["pairs"]],
                         [("list-000", F), ("list-000", A), ("list-001", F), ("list-002", F), ("list-003", F)])
        del verdicts[("list-000", A)]
        with self.assertRaises(SampleError):
            build_list(verdicts, holdout, "s", 4, [F, A])


class CliTests(unittest.TestCase):
    def test_writes_create_only_and_prints_the_pinned_sha(self):
        with tempfile.TemporaryDirectory() as tmp, mock.patch("sys.stderr", io.StringIO()):
            root, manifest, always = write_holdout(tmp, questions_per_history=2)
            folder = Path(tmp) / "verdicts"
            folder.mkdir()
            for qid, arm, score in (("h1-q01", F, "correct"), ("h1-q01", S, "incorrect"), ("h1-q02", F, "correct"),
                                    ("h1-q02", S, "correct")):
                (folder / f"{qid}.{arm}.json").write_text(json.dumps(verdict(qid, arm, score)))
            out_path = Path(tmp) / "list.json"
            argv = ["--verdicts-dir", str(folder), "--holdout-dir", str(root), "--holdout-manifest-sha256", manifest,
                    "--always50-sha256", always, "--seed", "s", "--sample-size", "1", "--sample-arms", F,
                    "--out", str(out_path)]
            out = io.StringIO()
            self.assertEqual(main(argv, out), 0)
            digest = hashlib.sha256(out_path.read_bytes()).hexdigest()
            self.assertTrue(out.getvalue().startswith(digest))
            doc = json.loads(out_path.read_text())
            self.assertIn({"question_id": "h1-q01", "arm": S}, doc["pairs"])  # the F/S regression
            self.assertEqual(main(argv, io.StringIO()), 2)  # create-only
            again = Path(tmp) / "again.json"
            self.assertEqual(main(argv[:-1] + [str(again)], io.StringIO()), 0)
            self.assertEqual(again.read_bytes(), out_path.read_bytes())
            self.assertEqual(main(argv[:-3] + ["bogus_arm", "--out", str(Path(tmp) / "x.json")], io.StringIO()), 2)
            included = Path(tmp) / "included.json"
            self.assertEqual(main(argv[:-1] + [str(included), "--include-pair", f"h1-q02:{S}"], io.StringIO()), 0)
            self.assertEqual(json.loads(included.read_text())["mandatory"]["included_pairs"], [["h1-q02", S]])
            self.assertEqual(main(argv[:-1] + [str(Path(tmp) / "y.json"), "--include-pair", "h1-q02"],
                                  io.StringIO()), 2)


if __name__ == "__main__":
    unittest.main()
