from decimal import Decimal
import hashlib
import itertools
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest import mock

from paid_call_journal import CallJournal
from paid_campaign import CampaignError
from paid_control import (RESERVE_CAP_USD, SYSTEM, control_binding, control_call, control_reservation,
                          has_settled_control)
from paid_fake_openai import FakeOpenAI
from paid_openai_wire import Rejected4xx, RunStopped, Transport
from paid_test_support import PROFILES, load_fixture_sheet, make_campaign

KEY = "fixture-only-key"
CONTROL_ROLE = {hashlib.sha256(SYSTEM.encode()).hexdigest(): "control"}


class ControlCallTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.sheet = load_fixture_sheet(self.directory.name)
        patch = mock.patch.dict(os.environ, {"PAID_TEST_KEY": KEY})
        patch.start()
        self.addCleanup(patch.stop)
        self.clock = itertools.count(1_700_000_000).__next__

    def campaign(self, **overrides):
        campaign = make_campaign(self.directory.name, self.sheet, **overrides)
        return campaign, CallJournal(campaign.directory)

    def test_reserves_at_most_cap_and_settles(self):
        campaign, journal = self.campaign()
        bound, max_usd = control_reservation(campaign, self.sheet)
        self.assertEqual((bound.stage, bound.model, bound.r_tokens, bound.o_tokens),
                         ("control", "gpt-6.2-astra", 9192, 4096))
        self.assertEqual(max_usd, Decimal("0.237376"))
        self.assertLessEqual(max_usd, RESERVE_CAP_USD)
        with FakeOpenAI({"control": "ok"}, model_by_role={"judge": "gpt-6.2-astra"},
                        role_by_system_sha256=CONTROL_ROLE, token=KEY) as fake:
            transport = Transport(fake.url, "PAID_TEST_KEY", {"127.0.0.1"})
            done = control_call(campaign, journal, self.sheet, transport, clock=self.clock)
            self.assertEqual(done.outcome, "ok")
            self.assertEqual(fake.log[0]["role"], "control")
            self.assertEqual(fake.log[0]["wire"]["max_completion_tokens"], 4096)
            self.assertEqual(fake.log[0]["wire"]["reasoning_effort"], "high")
        ledger = json.loads((Path(campaign.directory) / "budget.json").read_text())["requests"]
        self.assertEqual(len(ledger), 1)
        row = next(iter(ledger.values()))
        self.assertEqual(Decimal(row["max_usd"]), Decimal("0.237376"))
        self.assertIsNotNone(row["actual_usd"])
        self.assertLess(Decimal(row["actual_usd"]), Decimal(row["max_usd"]))
        intent = journal.state.intents[done.request_id]
        self.assertEqual((intent["letter"], intent["stage"], intent["binding"]["stage"]), ("K", "control", "control"))
        self.assertEqual(intent["binding"]["attempt"], 1)
        self.assertEqual(intent["binding"]["manifest_sha256"], campaign.spec["holdout"]["manifest_sha256"])
        self.assertTrue(has_settled_control(journal.state))
        # the identical control call replays from the journal: no new transport call, no new reservation
        replay = control_call(campaign, journal, self.sheet, transport, clock=self.clock)
        self.assertTrue(replay.replayed)
        self.assertEqual(campaign.ledger.totals()["request_count"], 1)

    def test_bad_request_400_is_a_pilot_stop(self):
        campaign, journal = self.campaign()
        with FakeOpenAI({"control": "bad_request_400"}, model_by_role={"judge": "gpt-6.2-astra"},
                        role_by_system_sha256=CONTROL_ROLE, token=KEY) as fake:
            transport = Transport(fake.url, "PAID_TEST_KEY", {"127.0.0.1"})
            with self.assertRaises(Rejected4xx):
                control_call(campaign, journal, self.sheet, transport, clock=self.clock)
            settled = next(iter(journal.state.settled.values()))
            self.assertEqual((settled["outcome"], settled["actual_usd"], settled["http_status"]), ("rejected_4xx", "0", 400))
            self.assertFalse(has_settled_control(journal.state))
            # no further control call is possible: the same frozen request cannot be replayed or re-sent
            with self.assertRaises(RunStopped):
                control_call(campaign, journal, self.sheet, transport, clock=self.clock)
            self.assertEqual(len(fake.log), 1)
        totals = campaign.ledger.totals()
        self.assertEqual((totals["request_count"], totals["spent_usd"]), (1, Decimal(0)))

    def test_refusals_before_any_reservation(self):
        profiles = {k: v for k, v in PROFILES.items() if k != "control"}
        campaign, journal = self.campaign(profiles=profiles)
        self.assertFalse(campaign.has_control_profile())
        with self.assertRaises(CampaignError):
            control_call(campaign, journal, self.sheet, transport=None)
        self.assertEqual(campaign.ledger.totals()["request_count"], 0)
        # P2 would reserve the whole published window of the judge model: far above 0.24 USD
        other = tempfile.TemporaryDirectory()
        self.addCleanup(other.cleanup)
        p2 = make_campaign(other.name, self.sheet, bound_policy="p2")
        with self.assertRaises(CampaignError) as refused:
            control_reservation(p2, self.sheet)
        self.assertIn("0.2374", str(refused.exception))
        with self.assertRaises(CampaignError):
            control_call(p2, CallJournal(p2.directory), self.sheet, transport=None)
        self.assertEqual(p2.ledger.totals()["request_count"], 0)

    def test_binding_is_fixed(self):
        campaign, _ = self.campaign()
        bind = control_binding(campaign)
        self.assertEqual({k: bind[k] for k in ("history_id", "session_id", "question_id", "arm", "stage", "attempt")},
                         {"history_id": None, "session_id": None, "question_id": None, "arm": None,
                          "stage": "control", "attempt": 1})
        self.assertEqual(bind["prompt_sha256"], hashlib.sha256(SYSTEM.encode()).hexdigest())
        self.assertEqual(len(bind["context_sha256"]), 64)


if __name__ == "__main__":
    unittest.main()
