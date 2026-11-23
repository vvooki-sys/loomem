import json
import os
from pathlib import Path
import shutil
import tempfile
import unittest

from paid_campaign import CampaignError, SublimitExceeded, create_campaign, open_campaign
from paid_test_support import load_fixture_sheet, make_campaign, spec


class CreateCampaignTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        self.target = self.root / "campaign"

    def assert_refused(self, document, sheet=None, errors=(CampaignError,)):
        with self.assertRaises(errors):
            create_campaign(self.target, document, sheet)
        self.assertFalse(self.target.exists())

    def test_invalid_specs_are_refused_without_writing(self):
        def without(key, section=None):
            document = spec()
            (document[section] if section else document).pop(key)
            return document

        def profile(stage, **changes):
            document = spec()
            document["profiles"][stage].update(changes)
            return document

        cases = {
            "no limit": without("limit_usd"),
            "no sublimit E": without("E", "sublimits_usd"),
            "no sublimit R": without("R", "sublimits_usd"),
            "sum over limit": spec(sublimits_usd={"E": "10", "F": "30", "S": "30", "A": "10", "R": "20.01", "K": "0.2374"}),
            "bad mode": spec(mode="free"),
            "p1 not accepted": spec(bound_policy="p1", p1_framing_accepted=False),
            "judge blind": spec(judge_sees_sources=False),
            "zero timeout": spec(timeout_s=0),
            "profile missing": without("review", "profiles"),
            "ultra effort": profile("answer", reasoning_effort="ultra"),
            "supplied id": spec(campaign_id="x"),
            "limit string junk": spec(limit_usd="ten"),
            "negative limit": spec(limit_usd="-1"),
        }
        for label, document in cases.items():
            with self.subTest(case=label):
                self.assert_refused(document)

    def test_sheet_cross_checks(self):
        sheet = load_fixture_sheet(self.root)
        judge512 = spec()
        judge512["profiles"]["judge"]["max_completion_tokens"] = 512
        self.assert_refused(judge512, sheet)
        unpriced = spec()
        unpriced["profiles"]["answer"]["model"] = "gpt-5-nova"
        self.assert_refused(unpriced, sheet, (CampaignError, ValueError))
        wrong_model = spec()
        wrong_model["profiles"]["answer"]["model"] = "gpt-6.2-astra"
        self.assert_refused(wrong_model, sheet)
        self.assertEqual(create_campaign(self.target, spec(), sheet).limit, 100)

    def test_create_is_write_once(self):
        campaign = make_campaign(self.root)
        self.assertTrue((self.target / "campaign.json").is_file())
        self.assertTrue((self.target / "budget.json").is_file())
        self.assertRegex(campaign.campaign_id, r"^[0-9a-f]{64}$")
        with self.assertRaises(CampaignError):
            make_campaign(self.root)
        self.assertEqual(json.loads((self.target / "campaign.json").read_text())["campaign_id"],
                         campaign.campaign_id)

    def test_campaign_id_is_unique_per_creation(self):
        # Plan §9: a recreated register must never alias a lost one. The same spec
        # created twice gets a fresh nonce, hence different campaign_ids (and request_ids).
        first = make_campaign(self.root / "a")
        second = make_campaign(self.root / "b")
        self.assertNotEqual(first.campaign_id, second.campaign_id)
        self.assertNotEqual(first.spec["nonce"], second.spec["nonce"])
        with self.assertRaises(CampaignError):
            make_campaign(self.root / "c", nonce="supplied")


class OpenCampaignTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        self.campaign = make_campaign(self.root)
        self.target = self.campaign.directory

    def test_open_returns_the_same_campaign(self):
        reopened = open_campaign(self.target)
        self.assertEqual(reopened.campaign_id, self.campaign.campaign_id)
        self.assertEqual(reopened.limit, self.campaign.limit)
        self.assertEqual(reopened.sublimits["F"], 30)
        self.assertEqual(reopened.policy, "p1")
        self.assertEqual(reopened.profile("judge")["max_completion_tokens"], 16000)
        self.assertEqual(open_campaign(self.target, load_fixture_sheet(self.root)).limit, 100)

    def test_modified_registry_is_refused(self):
        path = self.target / "campaign.json"
        document = json.loads(path.read_text())
        document["limit_usd"] = "200"
        path.write_text(json.dumps(document))
        with self.assertRaisesRegex(CampaignError, "modified"):
            open_campaign(self.target)
        path.write_text("{not json")
        with self.assertRaises(CampaignError):
            open_campaign(self.target)

    def test_missing_ledger_is_not_reset(self):
        (self.target / "budget.json").unlink()
        self.assertTrue((self.target / "budget.json.lock").is_file())
        with self.assertRaisesRegex(CampaignError, "ledger refused"):
            open_campaign(self.target)
        self.assertFalse((self.target / "budget.json").exists())

    def test_ledger_limit_must_match_registry(self):
        other = make_campaign(self.root / "other", limit_usd="50",
                              sublimits_usd={"E": "10", "F": "10", "S": "10", "A": "10", "R": "9", "K": "0.2374"})
        mixed = self.root / "mixed"
        mixed.mkdir()
        shutil.copy(self.target / "campaign.json", mixed / "campaign.json")
        for name in ("budget.json", "budget.json.lock"):
            shutil.copy(other.directory / name, mixed / name)
        with self.assertRaisesRegex(CampaignError, "ledger refused"):
            open_campaign(mixed)

    def test_sublimits_and_mode(self):
        self.campaign.check_sublimit("F", "29", "1")
        with self.assertRaises(SublimitExceeded):
            self.campaign.check_sublimit("F", "29", "1.01")
        with self.assertRaises(CampaignError):
            self.campaign.check_sublimit("X", "0", "0")
        self.campaign.require_paid_mode()
        dry = make_campaign(self.root / "dry", mode="dry_run")
        with self.assertRaises(CampaignError):
            dry.require_paid_mode()

    def test_run_lock_is_exclusive_and_survives_a_crash(self):
        lock = self.target / "run.lock"
        with self.campaign.run_lock() as held:
            self.assertEqual(held, lock)
            self.assertEqual(json.loads(lock.read_text())["pid"], os.getpid())
            with self.assertRaises(CampaignError):
                with open_campaign(self.target).run_lock():
                    pass
        self.assertFalse(lock.exists())
        lock.write_text("stale")
        with self.assertRaisesRegex(CampaignError, "run.lock present"):
            with self.campaign.run_lock():
                pass
        self.assertEqual(lock.read_text(), "stale")


if __name__ == "__main__":
    unittest.main()
