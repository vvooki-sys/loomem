import json
import os
from pathlib import Path
import tempfile
import unittest

import paid_holdout
from paid_holdout import EXECUTION_FIELDS, GOLD_FIELDS, Holdout, HoldoutError, leaks_gold, verify_holdout
from paid_test_support import question_row, sha256_bytes, write_holdout

HERE = Path(__file__).resolve().parent
QUESTIONS = "shards/a/questions.jsonl"
HISTORY = "shards/a/histories/h1.json"


class HoldoutTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root, self.manifest_sha, self.always_sha = write_holdout(self.directory.name)

    def verify(self, manifest_sha=None, always_sha=None):
        return verify_holdout(self.root, manifest_sha or self.manifest_sha, always_sha or self.always_sha)

    def rewrite_manifest(self, mutate):
        """Apply `mutate(manifest)` and re-pin; returns the new manifest sha."""
        path = self.root / "manifest.json"
        manifest = json.loads(path.read_text())
        mutate(manifest)
        path.write_bytes(json.dumps(manifest, sort_keys=True).encode("utf-8"))
        return sha256_bytes(path.read_bytes())

    def rewrite_listed(self, relative, data):
        """Rewrite a listed file and refresh its hash in the manifest; returns the new manifest sha."""
        (self.root / relative).write_bytes(data)

        def refresh(manifest):
            manifest["files_sha256"][relative] = sha256_bytes(data)
        return self.rewrite_manifest(refresh)

    def test_happy_path(self):
        holdout = self.verify()
        self.assertIsInstance(holdout, Holdout)
        self.assertEqual(holdout.question_order, ("h1-q01", "h1-q02"))
        self.assertIn("h1", holdout.histories)
        self.assertEqual(holdout.always_ids, frozenset({"h1-q01"}))
        self.assertEqual((len(holdout.session_bytes()), all(isinstance(b, int) for b in holdout.session_bytes())), (2, True))
        self.assertGreater(holdout.sources_bytes_max(), 0)
        self.assertEqual((holdout.manifest_sha256, holdout.always50_sha256), (self.manifest_sha, self.always_sha))

    def test_wrong_pins_and_missing_manifest(self):
        with self.assertRaises(HoldoutError):
            self.verify(manifest_sha="0" * 64)
        with self.assertRaises(HoldoutError):
            self.verify(always_sha="0" * 64)
        (self.root / "manifest.json").unlink()
        with self.assertRaises(HoldoutError):
            self.verify()

    def test_listed_file_changed_missing_or_tooling_modified(self):
        history = self.root / HISTORY
        history.write_bytes(history.read_bytes() + b"\n")
        with self.assertRaisesRegex(HoldoutError, "changed"):
            self.verify()
        history.unlink()
        with self.assertRaisesRegex(HoldoutError, "missing"):
            self.verify()
        self.root, self.manifest_sha, self.always_sha = write_holdout(Path(self.directory.name) / "two")
        with (self.root / "SPEC.md").open("a") as handle:
            handle.write("tampered\n")
        with self.assertRaises(HoldoutError):
            self.verify()

    def test_consistency_checks_after_repinning(self):
        def drop_question(manifest):
            manifest["ordered_question_ids"].remove("h1-q02")
        with self.assertRaises(HoldoutError):
            self.verify(manifest_sha=self.rewrite_manifest(drop_question))

        def wrong_history(manifest):
            manifest["ordered_question_ids"] = ["h1-q01", "h1-q02"]
            manifest["question_to_history"]["h1-q01"] = "h9"
        with self.assertRaises(HoldoutError):
            self.verify(manifest_sha=self.rewrite_manifest(wrong_history))

    def test_question_date_before_last_session(self):
        rows =[question_row("h1-q01", "h1", question_date="2020-01-01"), question_row("h1-q02", "h1")]
        data = ("\n".join(json.dumps(r) for r in rows) + "\n").encode("utf-8")
        with self.assertRaisesRegex(HoldoutError, "precedes"):
            self.verify(manifest_sha=self.rewrite_listed(QUESTIONS, data))

    def test_always_originals_checks(self):
        def drop_pin(manifest):
            manifest.pop("always_originals_50")
        unpinned = self.rewrite_manifest(drop_pin)
        self.assertEqual(self.verify(manifest_sha=unpinned).always_ids, frozenset({"h1-q01"}))

        def disagree(manifest):
            manifest["always_originals_50"] = {"file": "always_originals_50.json", "question_ids": ["h1-q02"],
                                               "sha256": self.always_sha}
        with self.assertRaises(HoldoutError):
            self.verify(manifest_sha=self.rewrite_manifest(disagree))
        apath = self.root / "always_originals_50.json"
        apath.write_text(json.dumps({"question_ids": ["zz-q99"], "seed": 1}))
        with self.assertRaises(HoldoutError):
            self.verify(manifest_sha=unpinned, always_sha=sha256_bytes(apath.read_bytes()))

    def test_views_and_gold_markers(self):
        holdout = self.verify()
        execution = holdout.execution_view("h1-q01")
        self.assertEqual(tuple(execution), EXECUTION_FIELDS)
        self.assertNotIn("SECRET", json.dumps(execution))
        judge = holdout.judge_view("h1-q01")
        self.assertEqual(tuple(judge), GOLD_FIELDS)
        self.assertEqual(judge["reference_answer"], "GOLD-REFERENCE-h1-q01-SECRET")
        with self.assertRaises(HoldoutError):
            holdout.execution_view("nope")
        with self.assertRaises(HoldoutError):
            holdout.judge_view("nope")
        markers = holdout.gold_markers("h1-q01")
        self.assertTrue(markers and all(len(m) >= 12 for m in markers))
        gold = {f"GOLD-{kind}-h1-q01-SECRET" for kind in ("REFERENCE", "ATOM", "RULE", "NOTES")}
        self.assertLessEqual(gold, set(markers))
        self.assertEqual(set(markers), gold)  # category is a judge field but not a leak marker
        self.assertEqual(leaks_gold(json.dumps(execution), markers), [])
        self.assertTrue(leaks_gold(json.dumps(judge), markers))

    def test_gold_strings_that_are_source_text_are_not_markers(self):
        """An evidence quote or an atom that occurs verbatim in a holdout session is source content
        (extraction requests and the originals menu carry it legitimately), never a leak marker."""
        holdout = self.verify()
        question = holdout.questions["h1-q01"]
        turn = holdout.histories["h1"]["sessions"][0]["turns"][0]["content"]
        question["evidence"] = [{"session_id": "h1-s1", "turn_index": 0, "quote": turn}]
        question["required_answer_atoms"] = [turn[:14], "GOLD-ATOM-h1-q01-SECRET"]
        markers = holdout.gold_markers("h1-q01")
        self.assertNotIn(turn, markers)
        self.assertNotIn(turn[:14], markers)
        self.assertIn("GOLD-ATOM-h1-q01-SECRET", markers)
        self.assertIn("GOLD-REFERENCE-h1-q01-SECRET", markers)
        report = holdout.gold_marker_report("h1-q01")
        self.assertEqual(report["evidence"], {"kept": 0, "excluded_as_source": 1})
        self.assertEqual(report["required_answer_atoms"], {"kept": 1, "excluded_as_source": 1})
        self.assertEqual(report["reference_answer"], {"kept": 1, "excluded_as_source": 0})
        from paid_holdout import KEY_MARKERS
        self.assertIn('"reference_answer"', KEY_MARKERS)
        self.assertEqual(leaks_gold(json.dumps(holdout.judge_view("h1-q01")), list(KEY_MARKERS)),
                         ['"reference_answer"', '"required_answer_atoms"', '"equivalence_rule"', '"unanswerable_explanation"'])

    def test_static_gold_blindness(self):
        self.assertFalse(set(paid_holdout.EXECUTION_FIELDS) & set(paid_holdout.GOLD_FIELDS))
        for name in ("paid_openai_wire.py", "paid_call_journal.py"):
            source = (HERE / name).read_text(encoding="utf-8")
            for marker in ("judge_view", "reference_answer"):
                self.assertNotIn(marker, source, f"{name} mentions {marker}")

    def test_verify_never_writes(self):
        def snapshot():
            return {p: (p.stat().st_mtime_ns, p.stat().st_size) for p in sorted(self.root.rglob("*")) if p.is_file()}
        before = snapshot()
        self.verify()
        self.assertEqual(snapshot(), before)
        self.assertEqual(sorted(os.listdir(self.root)), ["SPEC.md", "always_originals_50.json", "manifest.json", "shards"])


if __name__ == "__main__":
    unittest.main()
