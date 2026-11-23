"""The gold-leak scan reads the saved requests as the provider sees them (decoded message content)."""
import json
import tempfile
import unittest
from pathlib import Path

from paid_call_journal import JournalState, write_create_only
from paid_holdout import verify_holdout
from paid_preflight_probes import gold_scan
from test_paid_preflight import build_fixture


class GoldScanTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        fixture = build_fixture(self.root)
        self.holdout = verify_holdout(fixture.holdout, fixture.manifest_sha, fixture.always_sha)
        self.directory = self.root / "campaign"

    def scan(self, stage, user_content):
        self.calls = getattr(self, "calls", 0) + 1
        rid = f"{self.calls:064x}"
        state = JournalState(intents={rid: {"stage": stage, "letter": "F", "max_usd": "0"}})
        wire = {"model": "gpt-6-luna", "messages": [{"role": "system", "content": "SYNTHETIC answer SYSTEM"},
                                                    {"role": "user", "content": user_content}]}
        write_create_only(self.directory / "calls" / f"{rid}.request.json", wire)
        return gold_scan(self.directory, state, self.holdout)

    def test_gold_object_with_source_text_values_is_caught_by_its_keys(self):
        """Regression (Greptile): the file escapes quotes inside content, so `"reference_answer"` never
        matched the raw text; a gold object whose values are source text slipped through."""
        turn = self.holdout.histories["h1"]["sessions"][0]["turns"][0]["content"]
        leaked = json.dumps({"question": "q", "reference_answer": turn, "required_answer_atoms": [turn]})
        passed, detail = self.scan("answer", leaked)
        self.assertFalse(passed, detail)
        self.assertIn("(answer): 2 marker(s)", detail)

    def test_gold_value_is_caught_and_clean_request_passes(self):
        passed, detail = self.scan("answer", json.dumps({"evidence": "GOLD-REFERENCE-h1-q01-SECRET"}))
        self.assertFalse(passed, detail)
        clean = self.scan("diagnostic", json.dumps({"question": "Synthetic question h1-q01?", "facts": []}))
        self.assertTrue(clean[0], clean[1])
        self.assertIn("incl. 6 key names", clean[1])

    def test_judge_requests_are_not_scanned(self):
        passed, detail = self.scan("judge", json.dumps({"reference": {"reference_answer": "GOLD-REFERENCE-h1-q01-SECRET"}}))
        self.assertTrue(passed, detail)
        self.assertIn("0 non-judge request(s)", detail)


if __name__ == "__main__":
    unittest.main()
