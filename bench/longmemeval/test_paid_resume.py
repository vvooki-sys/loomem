"""Crash/resume semantics at the journal + wire level (no arm-level logic here)."""
import itertools
import json
import os
from pathlib import Path
import tempfile
import unittest

from paid_call_journal import CallJournal, JournalError, reconcile, request_id, sha256_hex
from paid_campaign import open_campaign
from paid_openai_wire import RunStopped, build_wire, paid_completion, wire_bytes
from paid_test_support import (SOURCES_BYTES_MAX, FakeTransport, binding, load_fixture_sheet, make_campaign,
                               messages, ok_body)


def ok(body=None):
    return (200, {"x-request-id": "prov-1"}, body if body is not None else ok_body())


def without_usage():
    body = json.loads(ok_body())
    del body["usage"]
    return json.dumps(body).encode("utf-8")


class ResumeTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.sheet = load_fixture_sheet(self.directory.name)
        self.bound = self.sheet.stage_profile("diagnostic", SOURCES_BYTES_MAX, True)
        self.clock = itertools.count(1_700_000_000).__next__
        os.environ["PAID_TEST_KEY"] = "fixture-only-key"
        self.addCleanup(os.environ.pop, "PAID_TEST_KEY", None)
        self.campaign = make_campaign(self.directory.name, self.sheet)
        self.journal = CallJournal(self.campaign.directory)
        self.calls = Path(self.campaign.directory) / "calls"

    def reopen(self):
        """Simulate a process restart: fresh campaign (ledger) and fresh journal from disk."""
        self.campaign = open_campaign(self.campaign.directory, self.sheet)
        self.journal = CallJournal(self.campaign.directory)
        return self.journal.state

    def call(self, transport, **kw):
        args = {"binding": binding(), "messages": messages(), "acknowledged": frozenset()}
        args.update(kw)
        return paid_completion(self.campaign, self.journal, self.sheet, letter="F", stage="diagnostic",
                               response_format=None, transport=transport, bound=self.bound, clock=self.clock, **args)

    def wire_for(self, msgs):
        return build_wire(self.campaign.profile("diagnostic"), msgs)

    def reserve_and_intent(self, bind=None, msgs=None, max_usd=None):
        """Manually reproduce check -> reserve -> intent for one request; returns (rid, token, wire)."""
        wire = self.wire_for(msgs or messages())
        wire_sha = sha256_hex(wire_bytes(wire))
        rid = request_id(self.campaign.campaign_id, bind or binding(), wire_sha)
        max_usd = max_usd or str(self.sheet.max_usd(self.bound, "p1"))
        token = self.campaign.ledger.reserve(rid, max_usd)
        self.journal.intent({"schema": 1, "request_id": rid, "campaign_id": self.campaign.campaign_id,
                             "created_at": self.clock(), "letter": "F", "stage": "diagnostic",
                             "binding": bind or binding(), "profile": self.campaign.profile("diagnostic"),
                             "bound_policy": "p1", "r_tokens": self.bound.r_tokens, "o_tokens": self.bound.o_tokens,
                             "max_usd": max_usd, "wire_sha256": wire_sha, "wire_bytes": len(wire_bytes(wire)),
                             "ledger_token": token}, wire)
        return rid, token, wire

    def test_crash_after_settled_replays(self):
        first = self.call(FakeTransport([ok()]))
        self.reopen()
        transport = FakeTransport([])
        again = self.call(transport)
        self.assertEqual((again.request_id, again.replayed), (first.request_id, True))
        self.assertEqual(transport.calls, [])
        self.assertEqual(self.campaign.ledger.totals()["request_count"], 1)
        reconcile(self.campaign.ledger.totals(), self.journal.state)

    def test_crash_after_intent_before_sent(self):
        rid, _, _ = self.reserve_and_intent()
        state = self.reopen()
        self.assertEqual(state.pending_intents, [rid])
        self.assertEqual(self.journal.void_unsent(self.clock()), [rid])
        self.assertEqual(state.pending_intents, [])
        self.assertEqual(self.campaign.ledger.totals()["pending_count"], 1)  # reservation stays, fail-closed
        transport = FakeTransport([ok()])
        with self.assertRaisesRegex(RunStopped, "attempted before"):
            self.call(transport)
        self.assertEqual(transport.calls, [])
        done = self.call(transport, binding=binding(attempt=2))
        self.assertNotEqual(done.request_id, rid)
        self.assertEqual((done.replayed, len(transport.calls)), (False, 1))
        self.assertEqual(self.campaign.ledger.totals()["request_count"], 2)

    def test_crash_after_sent_blocks_until_acknowledged(self):
        rid, _, _ = self.reserve_and_intent()
        self.journal.sent(rid, self.clock(), 5, "fake")
        state = self.reopen()
        self.assertEqual(state.unsettled_sent, [rid])
        transport = FakeTransport([ok()])
        other = messages(user="other question")
        with self.assertRaises(RunStopped):
            self.call(transport, binding=binding(qid="q2"), messages=other)
        self.assertEqual(transport.calls, [])
        done = self.call(transport, binding=binding(qid="q2"), messages=other, acknowledged={rid})
        self.assertEqual(len(transport.calls), 1)
        self.assertEqual(transport.calls[0][0], wire_bytes(self.wire_for(other)))
        self.assertNotEqual(done.request_id, rid)
        self.assertFalse((self.calls / f"{rid}.settled.json").exists())
        self.assertEqual(self.journal.state.unsettled_sent, [rid])

    def test_uncertain_is_never_resent(self):
        with self.assertRaises(RunStopped):
            self.call(FakeTransport([ok(without_usage())]))
        rid = next(iter(self.journal.state.uncertain))
        state = self.reopen()
        self.assertEqual((state.blocking(), list(state.uncertain)), ([rid], [rid]))
        transport = FakeTransport([ok()])
        with self.assertRaises(RunStopped):
            self.call(transport, binding=binding(qid="q2"))
        self.assertEqual(transport.calls, [])
        done = self.call(transport, binding=binding(qid="q2"), acknowledged={rid})
        self.assertEqual((done.outcome, len(transport.calls)), ("ok", 1))
        with self.assertRaisesRegex(RunStopped, "attempted before"):
            self.call(transport, acknowledged={rid})
        self.assertEqual(len(transport.calls), 1)
        self.assertEqual(self.campaign.ledger.totals()["pending_count"], 1)

    def test_changed_wire_for_same_binding_is_a_new_request(self):
        # The journal/wire layer only guarantees distinct request IDs for distinct frozen
        # input; the arm-level replay_or_call (B2-ii) is what raises FrozenInputChanged
        # when the same binding reappears with a different wire.
        transport = FakeTransport([ok(), ok()])
        first = self.call(transport)
        second = self.call(transport, messages=messages(user="changed input"))
        self.assertNotEqual(first.request_id, second.request_id)
        self.assertEqual((second.replayed, len(transport.calls)), (False, 2))
        for rid in (first.request_id, second.request_id):
            self.assertTrue((self.calls / f"{rid}.intent.json").exists())
            self.assertTrue((self.calls / f"{rid}.sent.json").exists())
        self.assertEqual(self.campaign.ledger.totals()["request_count"], 2)

    def test_ledger_journal_divergence_is_detected(self):
        self.call(FakeTransport([ok()]))
        reconcile(self.campaign.ledger.totals(), self.journal.state)
        self.campaign.ledger.reserve("f" * 64, "1")  # orphan: money reserved, no journal intent
        with self.assertRaisesRegex(JournalError, "orphaned reservation"):
            reconcile(self.campaign.ledger.totals(), self.journal.state)
        with self.assertRaises(JournalError):
            reconcile(self.campaign.ledger.totals(), self.reopen())


if __name__ == "__main__":
    unittest.main()
