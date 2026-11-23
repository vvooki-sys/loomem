from decimal import Decimal
import os
from pathlib import Path
import tempfile
import unittest

from paid_price_sheet import (CAP_NAMES, PriceUnknown, Sheet, SheetError, StageBound, load_sheet,
                              sha256_file)
from paid_test_support import SHEET_SOURCE, SOURCES_BYTES_MAX, ZERO64, load_fixture_sheet, write_sheet


class LoadSheetTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.path, self.digest = write_sheet(self.directory.name)

    def test_hash_pin_is_enforced(self):
        self.assertIsInstance(load_sheet(self.path, self.digest), Sheet)
        for bad in (ZERO64, "abc", None, self.digest.upper(), self.digest + "0"):
            with self.subTest(sha=bad), self.assertRaises(SheetError):
                load_sheet(self.path, bad)
        with self.assertRaises(SheetError):
            load_sheet(Path(self.directory.name) / "missing.py", self.digest)

    def test_missing_required_name_is_refused(self):
        source = SHEET_SOURCE.split("\n\ndef bounds")[0] + "\n"
        self.assertNotIn("def bounds", source)
        path, digest = write_sheet(self.directory.name, source)
        with self.assertRaisesRegex(SheetError, "bounds"):
            load_sheet(path, digest)

    def test_malformed_price_row_is_refused(self):
        source = SHEET_SOURCE.replace('"gpt-6.1-sol": (2.00, 10.00, 272_000, None, None)',
                                      '"gpt-6.1-sol": (2.00, 10.00, 272_000)')
        path, digest = write_sheet(self.directory.name, source)
        with self.assertRaisesRegex(SheetError, "5-tuple"):
            load_sheet(path, digest)

    def test_real_sheet_loads_when_pointed_at(self):
        real = os.environ.get("LOOMEM_COST_SHEET")
        if not real:
            self.skipTest("LOOMEM_COST_SHEET not set")
        sheet = load_sheet(real, sha256_file(real))
        self.assertEqual(set(sheet.caps()), set(CAP_NAMES))
        self.assertEqual(sheet.sha256, sha256_file(real))


class SheetTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.sheet = load_fixture_sheet(self.directory.name)
        self.judge = self.sheet.stage_profile("judge", SOURCES_BYTES_MAX, True)

    def test_unpriced_model_is_refused_everywhere(self):
        bound = StageBound("answer", "gpt-5-nova", 10, 10)
        for call in (lambda: self.sheet.require_priced("gpt-5-nova"),
                     lambda: self.sheet.standard_tier_limit("gpt-5-nova"),
                     lambda: self.sheet.context_window("gpt-5-nova"),
                     lambda: self.sheet.max_usd(bound, "p1"),
                     lambda: self.sheet.max_usd(bound, "p2")):
            with self.subTest(call=call), self.assertRaises(PriceUnknown):
                call()
        self.assertEqual(self.sheet.standard_tier_limit("gpt-6-luna"), 272_000)

    def test_judge_profile_matches_validation_sheet_numbers(self):
        self.assertEqual((self.judge.r_tokens, self.judge.o_tokens, self.judge.model),
                         (84_865, 16_000, "gpt-6.2-astra"))
        self.assertEqual(self.judge.stage, "judge")
        self.assertEqual(self.sheet.stage_profile("judge", SOURCES_BYTES_MAX, False).r_tokens,
                         84_865 - SOURCES_BYTES_MAX)

    def test_max_usd_is_exact_decimal_per_policy(self):
        p1 = self.sheet.max_usd(self.judge, "p1")
        p2 = self.sheet.max_usd(self.judge, "p2")
        self.assertIsInstance(p1, Decimal)
        self.assertIsInstance(p2, Decimal)
        self.assertEqual(p1, Decimal("1.31892"))
        self.assertEqual(p2, Decimal("15.712"))
        self.assertEqual(p1, Decimal(84_865) * Decimal("8") / Decimal(1_000_000)
                         + Decimal(16_000) * Decimal("40") / Decimal(1_000_000))
        with self.assertRaises(SheetError):
            self.sheet.max_usd(self.judge, "p3")

    def test_policies_refuse_unpublished_window_or_tier(self):
        sol = StageBound("answer", "gpt-6.1-sol", 1_000, 100)
        self.assertIsNone(self.sheet.context_window("gpt-6.1-sol"))
        with self.assertRaises(PriceUnknown):
            self.sheet.max_usd(sol, "p2")
        self.assertEqual(self.sheet.max_usd(sol, "p1"), Decimal("0.003"))
        over = StageBound("answer", "gpt-6-luna", 272_001, 100)
        with self.assertRaises(PriceUnknown):
            self.sheet.max_usd(over, "p1")
        self.assertEqual(self.sheet.max_usd(StageBound("answer", "gpt-6-luna", 272_000, 0), "p1"),
                         Decimal("0.0272"))
        self.assertEqual(self.sheet.max_usd(StageBound("answer", "gpt-6-luna", 1, 0), "p2"),
                         Decimal(1_050_000) * Decimal("0.20") / Decimal(1_000_000))

    def test_extraction_bound_is_per_session(self):
        bound = self.sheet.extraction_bound(1_000)
        self.assertEqual((bound.stage, bound.model, bound.r_tokens, bound.o_tokens),
                         ("extraction", "gpt-6-luna", 6_000 + 1_000 + 8_192, 16_384))
        self.assertEqual(self.sheet.extraction_bound(0).r_tokens, 6_000 + 8_192)
        for bad in (-1, 1.5, "1000", True, None):
            with self.subTest(session_bytes=bad), self.assertRaises(SheetError):
                self.sheet.extraction_bound(bad)
        with self.assertRaises(SheetError):
            self.sheet.stage_profile("extraction", SOURCES_BYTES_MAX, True)
        with self.assertRaises(SheetError):
            self.sheet.stage_profile("ingest", SOURCES_BYTES_MAX, True)

    def test_output_bound_depends_on_judge_model(self):
        self.assertEqual(self.sheet.output_bound("judge", "gpt-6.2-astra"), 16_000)
        self.assertEqual(self.sheet.output_bound("judge", "gpt-6-luna"), 512)
        self.assertEqual(self.sheet.output_bound("review", "gpt-6.2-astra"), 16_000)
        self.assertEqual(self.sheet.output_bound("diagnostic", "gpt-6.2-astra"), 1_024)
        with self.assertRaises(SheetError):
            self.sheet.output_bound("ingest", "gpt-6-luna")

    def test_static_tables(self):
        caps = self.sheet.caps()
        self.assertEqual(set(caps), set(CAP_NAMES))
        self.assertEqual(len(caps), 6)
        self.assertEqual(caps["QUESTION_CAP"], 2_048)
        self.assertEqual(self.sheet.framing, 8_192)
        self.assertEqual(self.sheet.models()["judge"], "gpt-6.2-astra")
        self.assertEqual(set(self.sheet.prices), {"gpt-6-luna", "gpt-6.1-sol", "gpt-6.2-astra"})
        self.assertAlmostEqual(self.sheet.float_cost("gpt-6-luna", 100_000, 0), 0.01)
        self.assertIsNone(self.sheet.float_cost("gpt-6.1-sol", 272_001, 0))


if __name__ == "__main__":
    unittest.main()
