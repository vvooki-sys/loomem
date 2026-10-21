import concurrent.futures
from decimal import Decimal
import json
import multiprocessing
import os
from pathlib import Path
import stat
import tempfile
import unittest
from unittest.mock import patch

from budget import BudgetBreach, BudgetError, BudgetExceeded, BudgetLedger, DuplicateReservation


def competing_reservation(args):
    path, request_id = args
    ledger = BudgetLedger(path, "1")
    try:
        ledger.reserve(request_id, "0.1")
        return True
    except BudgetExceeded:
        return False


class BudgetTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.path = Path(self.directory.name) / "budget.json"
        self.ledger = BudgetLedger(self.path, "100")

    def test_exact_reserve_settle_and_reopen(self):
        first = self.ledger.reserve("first", "99.9")
        self.ledger.reserve("uncertain", "0.1")
        self.assertEqual(self.ledger.totals()["remaining_usd"], Decimal(0))
        with self.assertRaises(BudgetExceeded):
            self.ledger.reserve("excess", "0.000000000001")
        self.ledger.settle(first, "0.2")
        reopened = BudgetLedger(self.path, "100.0")
        totals = reopened.totals()
        self.assertEqual(totals["spent_usd"], Decimal("0.2"))
        self.assertEqual(totals["reserved_usd"], Decimal("0.1"))
        self.assertEqual(totals["remaining_usd"], Decimal("99.7"))
        self.assertEqual(totals["pending_count"], 1)

    def test_request_ids_never_allow_blind_retries(self):
        token = self.ledger.reserve("once", "80")
        with self.assertRaises(DuplicateReservation):
            BudgetLedger(self.path, "100").reserve("once", "80")
        self.ledger.settle(token, "1")
        self.ledger.settle(token, "1.0")
        with self.assertRaises(DuplicateReservation):
            self.ledger.reserve("once", "80")
        with self.assertRaises(BudgetError):
            self.ledger.settle(token, "0")
        with self.assertRaises(BudgetError):
            self.ledger.settle("unknown", "0")
        self.assertEqual(self.ledger.totals()["spent_usd"], Decimal(1))

    def test_cap_is_immutable_and_missing_ledger_is_not_reset(self):
        self.ledger.reserve("pending", "100")
        for cap in ["99", "101"]:
            with self.assertRaises(BudgetError):
                BudgetLedger(self.path, cap)
        self.path.unlink()
        with self.assertRaises(BudgetError):
            BudgetLedger(self.path, "100")

    def test_invalid_money_is_never_accepted(self):
        for value in [True, None, [], "NaN", "sNaN", "Infinity", "-Infinity", -1,
                      float("nan"), float("inf"), "bad", "1e-10000000", "1e10000000"]:
            with self.subTest(value=value):
                with self.assertRaises(ValueError):
                    self.ledger.reserve("invalid", value)
                with self.assertRaises(ValueError):
                    self.ledger.settle("invalid", value)
                with self.assertRaises(ValueError):
                    BudgetLedger(self.path, value)
        self.assertEqual(self.ledger.totals()["request_count"], 0)

    def test_over_reservation_records_actual_then_freezes(self):
        token = self.ledger.reserve("bad-estimate", "1")
        with self.assertRaises(BudgetBreach):
            self.ledger.settle(token, "101")
        reopened = BudgetLedger(self.path, "100")
        self.assertEqual(reopened.totals()["spent_usd"], Decimal(101))
        self.assertEqual(reopened.totals()["remaining_usd"], Decimal(-1))
        self.assertTrue(reopened.totals()["breached"])
        with self.assertRaises(BudgetBreach):
            reopened.reserve("next", "0")

    def test_uncertain_reservation_survives_crash_and_blocks_budget(self):
        context = multiprocessing.get_context("fork")
        def child():
            BudgetLedger(self.path, "100").reserve("sent-before-crash", "100")
            os._exit(0)
        process = context.Process(target=child)
        process.start()
        process.join(10)
        self.assertEqual(process.exitcode, 0)
        reopened = BudgetLedger(self.path, "100")
        self.assertEqual(reopened.totals()["reserved_usd"], Decimal(100))
        with self.assertRaises(BudgetExceeded):
            reopened.reserve("other", "0.01")

    def test_multi_process_reservations_cannot_overspend(self):
        path = Path(self.directory.name) / "concurrent.json"
        with concurrent.futures.ProcessPoolExecutor(
                max_workers=6, mp_context=multiprocessing.get_context("fork")) as pool:
            results = list(pool.map(competing_reservation, [(path, str(i)) for i in range(30)]))
        self.assertEqual(sum(results), 10)
        self.assertEqual(BudgetLedger(path, "1").totals()["committed_usd"], Decimal(1))

    def test_thread_reservations_are_serialized(self):
        with concurrent.futures.ThreadPoolExecutor(max_workers=8) as pool:
            tokens = list(pool.map(lambda i: self.ledger.reserve(str(i), "1"), range(100)))
        self.assertEqual(len(set(tokens)), 100)
        self.assertEqual(self.ledger.totals()["reserved_usd"], Decimal(100))

    def test_replace_failure_preserves_previous_reservations(self):
        self.ledger.reserve("existing", "30")
        with patch("budget.os.replace", side_effect=OSError("simulated crash before replace")):
            with self.assertRaises(OSError):
                self.ledger.reserve("never-sent", "40")
        self.assertEqual(BudgetLedger(self.path, "100").totals()["reserved_usd"], Decimal(30))

    def test_settlement_write_failure_keeps_worst_case_reserved(self):
        token = self.ledger.reserve("sent", "90")
        with patch("budget.os.replace", side_effect=OSError("settlement interrupted")):
            with self.assertRaises(OSError):
                self.ledger.settle(token, "1")
        reopened = BudgetLedger(self.path, "100")
        self.assertEqual(reopened.totals()["reserved_usd"], Decimal(90))
        reopened.settle(token, "1")
        self.assertEqual(reopened.totals()["spent_usd"], Decimal(1))

    def test_directory_sync_failure_retains_written_reservation(self):
        real_fsync = os.fsync
        def fail_directory(descriptor):
            if stat.S_ISDIR(os.fstat(descriptor).st_mode):
                raise OSError("simulated uncertain fsync")
            return real_fsync(descriptor)
        with patch("budget.os.fsync", side_effect=fail_directory):
            with self.assertRaises(OSError):
                self.ledger.reserve("uncertain-write", "70")
        self.assertEqual(BudgetLedger(self.path, "100").totals()["reserved_usd"], Decimal(70))
        with self.assertRaises(DuplicateReservation):
            self.ledger.reserve("uncertain-write", "70")

    def test_corrupt_or_duplicate_key_ledger_fails_closed(self):
        for contents in ["", "not json", '{"schema":1,"schema":1}',
                         '{"schema":1,"limit_usd":"100","requests":[]}']:
            self.path.write_text(contents)
            with self.assertRaises(BudgetError):
                BudgetLedger(self.path, "100")

    def test_tampered_negative_settlement_is_rejected(self):
        token = self.ledger.reserve("one", "1")
        state = json.loads(self.path.read_text())
        state["requests"]["one"]["actual_usd"] = "-1"
        self.path.write_text(json.dumps(state))
        with self.assertRaises(BudgetError):
            self.ledger.settle(token, "0")

    def test_cached_reservation_binding_is_exact_and_read_only(self):
        token = self.ledger.reserve("cached-request", "2")
        before = self.path.read_bytes()
        self.ledger.verify_reservation(token, "cached-request", "2.0")
        self.assertEqual(self.path.read_bytes(), before)
        for candidate in [("forged", "cached-request", "2"),
                          (token, "other-request", "2"),
                          (token, "cached-request", "1"),
                          (token, "cached-request", "3")]:
            with self.assertRaises(BudgetError):
                self.ledger.verify_reservation(*candidate)
        other = BudgetLedger(Path(self.directory.name) / "other.json", "100")
        with self.assertRaises(BudgetError):
            other.verify_reservation(token, "cached-request", "2")
        self.assertEqual(self.path.read_bytes(), before)

    def test_cached_settlement_cannot_underreport_existing_charge(self):
        token = self.ledger.reserve("cached-request", "2")
        self.ledger.settle(token, "1.5")
        reopened = BudgetLedger(self.path, "100")
        reopened.verify_reservation(token, "cached-request", "2")
        with self.assertRaises(BudgetError):
            reopened.settle(token, "0.01")
        self.assertEqual(reopened.totals()["spent_usd"], Decimal("1.5"))

    def test_zero_reservation_and_empty_id_validation(self):
        token = self.ledger.reserve("free", 0)
        self.ledger.settle(token, 0)
        for invalid in [None, 1, "", "   "]:
            with self.assertRaises(ValueError):
                self.ledger.reserve(invalid, "0")
        self.assertEqual(self.ledger.totals()["remaining_usd"], Decimal(100))


if __name__ == "__main__":
    unittest.main()
