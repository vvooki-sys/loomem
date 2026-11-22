from decimal import Decimal
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

from paid_call_journal import (CallJournal, JournalError, JournalState, ModelDrift, reconcile,
                               request_id, write_create_only)
from paid_test_support import ZERO64, binding

ONE64 = "1" * 64


def intent_entry(rid, letter="F", max_usd="1.5"):
    return {"request_id": rid, "letter": letter, "stage": "diagnostic", "max_usd": max_usd,
            "wire_sha256": ZERO64, "ledger_token": "tok-" + rid, "profile": {"model": "gpt-6-luna"}}


def settled_result(actual="0.25", model="gpt-6-luna"):
    return {"http_status": 200, "model_observed": model, "usage": {"prompt_tokens": 1, "completion_tokens": 1},
            "finish_reason": "stop", "outcome": "ok", "actual_usd": actual}


class RequestIdTests(unittest.TestCase):
    def test_deterministic_and_input_sensitive(self):
        rid = request_id("camp", binding(), ZERO64)
        self.assertRegex(rid, r"^[0-9a-f]{64}$")
        self.assertEqual(rid, request_id("camp", binding(), ZERO64))
        self.assertNotEqual(rid, request_id("camp", binding(qid="q2"), ZERO64))
        self.assertNotEqual(rid, request_id("camp", binding(), ONE64))
        self.assertNotEqual(rid, request_id("other", binding(), ZERO64))
        for args in (("", binding(), ZERO64), (None, binding(), ZERO64), ("camp", {}, ZERO64),
                     ("camp", "b", ZERO64), ("camp", binding(), "abc"), ("camp", binding(), None)):
            with self.subTest(args=args), self.assertRaises(JournalError):
                request_id(*args)


class WriteCreateOnlyTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.path = Path(self.directory.name) / "calls" / "r.intent.json"

    def leftovers(self):
        return [p.name for p in self.path.parent.iterdir() if p.name.startswith(".")]

    def test_second_write_is_refused_and_original_kept(self):
        write_create_only(self.path, {"v": 1})
        with self.assertRaises(JournalError):
            write_create_only(self.path, {"v": 2})
        self.assertEqual(json.loads(self.path.read_text()), {"v": 1})
        self.assertEqual(self.leftovers(), [])

    def test_crash_between_temp_and_link_leaves_nothing(self):
        with patch("paid_call_journal.os.link", side_effect=OSError("disk gone")):
            with self.assertRaises(OSError):
                write_create_only(self.path, {"v": 1})
        self.assertFalse(self.path.exists())
        self.assertEqual(self.leftovers(), [])


class CallJournalTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        self.journal = CallJournal(self.root)
        self.rid = request_id("camp", binding(), ZERO64)

    def test_intent_validation(self):
        for key in ("letter", "stage", "max_usd", "wire_sha256", "ledger_token", "profile"):
            entry = intent_entry(self.rid)
            del entry[key]
            with self.subTest(missing=key), self.assertRaises(JournalError):
                self.journal.intent(entry, {})
        with self.assertRaises(JournalError):
            self.journal.intent(intent_entry(self.rid, letter="X"), {})
        self.assertEqual(self.journal.state.intents, {})
        self.journal.intent(intent_entry(self.rid), {"model": "gpt-6-luna"})
        with self.assertRaises(JournalError):
            self.journal.intent(intent_entry(self.rid), {})

    def test_transitions_are_ordered(self):
        with self.assertRaises(JournalError):
            self.journal.sent(self.rid, "t0", 5, "h")
        self.journal.intent(intent_entry(self.rid), {})
        with self.assertRaises(JournalError):
            self.journal.settled(self.rid, settled_result(), {})
        with self.assertRaises(JournalError):
            self.journal.uncertain(self.rid, "t1", "timeout")
        self.journal.sent(self.rid, "t0", 5, "h")
        with self.assertRaises(JournalError):
            self.journal.void(self.rid, "t1")
        incomplete = {k: v for k, v in settled_result().items() if k != "usage"}
        with self.assertRaises(JournalError):
            self.journal.settled(self.rid, incomplete, {})
        self.assertEqual(self.journal.state.settled, {})

    def test_happy_path_reloads_and_replays(self):
        self.journal.intent(intent_entry(self.rid), {"model": "gpt-6-luna"})
        self.journal.sent(self.rid, "t0", 5, "h")
        self.journal.settled(self.rid, settled_result(), {"id": "chatcmpl-1"})
        fresh = CallJournal(self.root)
        self.assertEqual(fresh.state.intents, {self.rid: intent_entry(self.rid)})
        self.assertEqual(fresh.state.sent, {self.rid})
        self.assertEqual(fresh.state.settled[self.rid], {"request_id": self.rid, **settled_result()})
        self.assertEqual(fresh.replay(self.rid), (fresh.state.settled[self.rid], {"id": "chatcmpl-1"}))
        self.assertIsNone(fresh.replay("unknown"))
        self.assertEqual(fresh.state.pending_intents, [])
        self.assertEqual(fresh.state.blocking(), [])

    def test_void_unsent_only_touches_pending(self):
        a, b, c = "a" * 64, "b" * 64, "c" * 64
        for rid in (a, b, c):
            self.journal.intent(intent_entry(rid), {})
        self.journal.sent(b, "t0", 5, "h")
        self.assertEqual(self.journal.state.pending_intents, [a, c])
        self.assertEqual(self.journal.void_unsent("t1"), [a, c])
        self.assertEqual(self.journal.state.pending_intents, [])
        self.assertEqual(self.journal.state.unsettled_sent, [b])
        for rid in (a, c):
            self.assertEqual(json.loads((self.root / "calls" / f"{rid}.void.json").read_text())["reason"],
                             "intent_without_sent")
        self.assertFalse((self.root / "calls" / f"{b}.void.json").exists())
        self.assertEqual(CallJournal(self.root).state.void.keys(), {a, c})

    def test_pin_model(self):
        pin = self.journal.pin_model("luna", "gpt-6-luna-2026", self.rid, "t0")
        self.assertEqual(pin["model"], "gpt-6-luna-2026")
        self.assertEqual(json.loads((self.root / "pins" / "luna.json").read_text()), pin)
        self.assertEqual(self.journal.pin_model("luna", "gpt-6-luna-2026", "other", "t1"), pin)
        for observed in ("gpt-6-luna-2027", "", None):
            with self.subTest(observed=observed), self.assertRaises(ModelDrift):
                self.journal.pin_model("luna", observed, self.rid, "t2")
        self.assertEqual(CallJournal(self.root).state.pins, {"luna": pin})

    def test_load_fails_closed_on_inconsistency(self):
        calls = self.root / "calls"
        cases = {"sent without intent": {"x.sent.json": {}},
                 "settled without sent": {"y.intent.json": intent_entry("y"), "y.settled.json": settled_result()},
                 "unexpected suffix": {"z.weird.json": {}},
                 "settled and uncertain": {"w.intent.json": intent_entry("w"), "w.sent.json": {},
                                           "w.settled.json": settled_result(), "w.uncertain.json": {}}}
        for label, files in cases.items():
            with self.subTest(case=label):
                for name, content in files.items():
                    (calls / name).write_text(json.dumps(content))
                with self.assertRaises(JournalError):
                    CallJournal(self.root)
                for name in files:
                    (calls / name).unlink()
        self.assertEqual(CallJournal(self.root).state.intents, {})


class JournalStateTests(unittest.TestCase):
    def setUp(self):
        self.state = JournalState()
        # p pending; s settled; n settled without usage; u uncertain; v voided; m sent, unsettled
        self.state.intents = {"p": intent_entry("p", "E", "1.5"), "s": intent_entry("s", "E", "2"),
                              "n": intent_entry("n", "F", "3"), "u": intent_entry("u", "A", "0.5"),
                              "v": intent_entry("v", "R", "4"), "m": intent_entry("m", "S", "0.75")}
        self.state.sent = {"s", "n", "u", "m"}
        self.state.settled = {"s": {"actual_usd": "0.25"}, "n": {"actual_usd": None}}
        self.state.uncertain = {"u": {"error_type": "timeout"}}
        self.state.void = {"v": {"reason": "intent_without_sent"}}

    def test_properties(self):
        self.assertEqual(self.state.pending_intents, ["p"])
        self.assertEqual(self.state.unsettled_sent, ["m"])
        self.assertTrue(self.state.has_uncertain)
        self.assertEqual(self.state.blocking(), ["m", "u"])
        self.assertEqual(self.state.blocking({"u"}), ["m"])
        self.assertEqual(self.state.blocking(frozenset({"nobody"})), ["m", "u"])

    def test_committed_sums(self):
        self.assertEqual(self.state.committed_by_letter(),
                         {"E": Decimal("1.75"), "F": Decimal("3"), "S": Decimal("0.75"),
                          "A": Decimal("0.5"), "R": Decimal("4")})
        self.assertEqual(self.state.committed_total(), Decimal("10"))

    def test_reconcile_against_ledger_totals(self):
        totals = {"request_count": 6, "spent_usd": Decimal("0.25"), "reserved_usd": Decimal("9.75")}
        reconcile(totals, self.state)
        for bad in ({"request_count": 7}, {"spent_usd": Decimal("0.5")}, {"reserved_usd": Decimal("9.7")}):
            with self.subTest(bad=bad), self.assertRaises(JournalError):
                reconcile({**totals, **bad}, self.state)


if __name__ == "__main__":
    unittest.main()
