"""Per-letter sublimits are enforced from journal committed sums before any reservation."""
from decimal import Decimal
import itertools
import os
from pathlib import Path
import tempfile
import unittest

from paid_call_journal import LETTERS, CallJournal, request_id, sha256_hex
from paid_campaign import SublimitExceeded
from paid_openai_wire import build_wire, paid_completion, wire_bytes
from paid_test_support import (SOURCES_BYTES_MAX, FakeTransport, binding, load_fixture_sheet, make_campaign,
                               messages, ok_body)

OK = (200, {"x-request-id": "prov-1"}, ok_body())


class SublimitTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.sheet = load_fixture_sheet(self.directory.name)
        self.bound = self.sheet.stage_profile("diagnostic", SOURCES_BYTES_MAX, True)
        self.max_usd = self.sheet.max_usd(self.bound, "p1")
        self.clock = itertools.count(1_700_000_000).__next__
        os.environ["PAID_TEST_KEY"] = "fixture-only-key"
        self.addCleanup(os.environ.pop, "PAID_TEST_KEY", None)

    def open(self, name="campaign", **overrides):
        self.campaign = make_campaign(Path(self.directory.name) / name, self.sheet, **overrides)
        self.journal = CallJournal(self.campaign.directory)
        return self.campaign

    def call(self, transport, letter, **kw):
        args = {"binding": binding(), "messages": messages()}
        args.update(kw)
        return paid_completion(self.campaign, self.journal, self.sheet, letter=letter, stage="diagnostic",
                               response_format=None, transport=transport, bound=self.bound, clock=self.clock, **args)

    def test_letter_sublimit_stops_before_reserve_despite_free_global_limit(self):
        self.open("f", sublimits_usd={"E": "10", "F": "0.000001", "S": "30", "A": "10", "R": "10", "K": "0.2374"})
        self.assertEqual(self.max_usd, Decimal("0.0049728"))
        transport = FakeTransport([OK])
        with self.assertRaises(SublimitExceeded):
            self.call(transport, "F")
        self.assertEqual(transport.calls, [])
        totals = self.campaign.ledger.totals()
        self.assertEqual((totals["request_count"], totals["remaining_usd"]), (0, Decimal("100")))
        self.assertEqual(self.journal.state.intents, {})
        done = self.call(transport, "S")
        self.assertEqual((done.outcome, len(transport.calls)), ("ok", 1))
        self.assertEqual(self.campaign.ledger.totals()["request_count"], 1)

    def test_margin_between_limit_and_sublimits_is_unusable(self):
        self.open("margin", limit_usd="100", sublimits_usd={letter: "0.000002" for letter in LETTERS})
        transport = FakeTransport([OK])
        for letter in LETTERS:
            with self.subTest(letter=letter), self.assertRaises(SublimitExceeded):
                self.call(transport, letter, binding=binding(qid=letter))
        self.assertEqual(transport.calls, [])
        totals = self.campaign.ledger.totals()
        self.assertEqual((totals["request_count"], totals["remaining_usd"]), (0, Decimal("100")))

    def test_committed_sums_include_unsettled_intents(self):
        self.open("committed")  # F sublimit is 30
        wire = build_wire(self.campaign.profile("diagnostic"), messages(user="earlier"))
        wire_sha = sha256_hex(wire_bytes(wire))
        rid = request_id(self.campaign.campaign_id, binding(qid="q0"), wire_sha)
        token = self.campaign.ledger.reserve(rid, "29.999")
        self.journal.intent({"request_id": rid, "letter": "F", "stage": "diagnostic", "max_usd": "29.999",
                             "wire_sha256": wire_sha, "ledger_token": token,
                             "profile": self.campaign.profile("diagnostic")}, wire)
        self.assertEqual(self.journal.state.committed_by_letter()["F"], Decimal("29.999"))
        transport = FakeTransport([OK])
        with self.assertRaisesRegex(SublimitExceeded, "letter F"):
            self.call(transport, "F", binding=binding(qid="q2"))
        self.assertEqual(transport.calls, [])
        self.assertEqual(self.campaign.ledger.totals()["request_count"], 1)
        done = self.call(transport, "S", binding=binding(qid="q2"))  # S's own sublimit is untouched
        self.assertEqual((done.outcome, len(transport.calls)), ("ok", 1))


if __name__ == "__main__":
    unittest.main()
