import hashlib
import itertools
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest import mock

from paid_call_journal import CallJournal
from paid_fake_openai import FakeOpenAI
from paid_input_tokens import InputTokenCounter, load_counts, parse_input_tokens
from paid_openai_wire import InputTokensBreach, Transport, WireError, build_wire, paid_completion
from paid_test_support import SOURCES_BYTES_MAX, binding, load_fixture_sheet, make_campaign, messages

KEY = "fixture-only-key"


class InputTokenCounterTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.sheet = load_fixture_sheet(self.directory.name)
        patch = mock.patch.dict(os.environ, {"PAID_TEST_KEY": KEY})
        patch.start()
        self.addCleanup(patch.stop)
        self.campaign = make_campaign(self.directory.name, self.sheet)
        self.journal = CallJournal(self.campaign.directory)
        self.clock = itertools.count(1_700_000_000).__next__
        self.bound = self.sheet.stage_profile("diagnostic", SOURCES_BYTES_MAX, True)

    def counter(self, fake, **context):
        system_sha = hashlib.sha256(messages()[0]["content"].encode()).hexdigest()
        return InputTokenCounter(self.campaign, self.campaign.directory, fake.url, "PAID_TEST_KEY",
                                 context={"arm": "facts_retry", "question_id": "q1", "history_id": "h1", **context},
                                 clock=self.clock, stage_by_system_sha256={system_sha: "diagnostic"})

    def test_returns_fake_count_and_records_probe(self):
        with FakeOpenAI({"default": "ok"}, model_by_role={"default": "gpt-6-luna"}, token=KEY) as fake:
            counter = self.counter(fake)
            wire = build_wire(self.campaign.profile("diagnostic"), messages())
            count = counter(wire)
            self.assertEqual(count, fake.count_log[-1]["input_tokens"])
            self.assertGreater(count, 0)
            # the identical frozen wire in the same context is counted again without a duplicate record error
            self.assertEqual(counter(wire), count)
        records = load_counts(self.campaign.directory)
        self.assertEqual(len(records), 1)
        self.assertEqual(records[0]["input_tokens"], count)
        self.assertEqual((records[0]["stage"], records[0]["arm"], records[0]["question_id"]),
                         ("diagnostic", "facts_retry", "q1"))
        self.assertEqual(records[0]["model"], "gpt-6-luna")
        self.assertEqual(len(records[0]["wire_sha256"]), 64)
        self.assertEqual(len(list((Path(self.campaign.directory) / "counts").glob("*.json"))), 1)

    def test_set_context_changes_the_record(self):
        with FakeOpenAI({"default": "ok"}, model_by_role={"default": "gpt-6-luna"}, token=KEY) as fake:
            counter = self.counter(fake)
            counter.set_context(stage="extraction", history_id="h9", session_id="s1", r_tokens=123)
            wire = build_wire(self.campaign.profile("diagnostic"), [messages()[0], {"role": "user", "content": "x"}])
            wire["messages"][0]["content"] = "an engine system prompt"
            counter(wire)
            with self.assertRaises(WireError):
                counter.set_context(bogus=1)
        record = load_counts(self.campaign.directory)[0]
        self.assertEqual((record["stage"], record["history_id"], record["session_id"], record["r_tokens"]),
                         ("extraction", "h9", "s1", 123))

    def test_over_bound_count_stops_before_any_reservation(self):
        with FakeOpenAI({"default": "ok"}, model_by_role={"default": "gpt-6-luna"}, token=KEY,
                        count_scenarios={"default": "count_over_bound"}) as fake:
            transport = Transport(fake.url, "PAID_TEST_KEY", {"127.0.0.1"})
            with self.assertRaises(InputTokensBreach):
                paid_completion(self.campaign, self.journal, self.sheet, letter="F", stage="diagnostic",
                                binding=binding(), messages=messages(), response_format=None, transport=transport,
                                bound=self.bound, clock=self.clock, precount=self.counter(fake))
            self.assertEqual(fake.log, [])  # no chat call was made
        self.assertEqual(self.campaign.ledger.totals()["request_count"], 0)
        self.assertEqual(self.journal.state.intents, {})
        self.assertEqual(load_counts(self.campaign.directory)[0]["input_tokens"], 10 ** 7)

    def test_precount_feeds_a_normal_call(self):
        with FakeOpenAI({"default": "ok"}, model_by_role={"default": "gpt-6-luna"}, token=KEY) as fake:
            transport = Transport(fake.url, "PAID_TEST_KEY", {"127.0.0.1"})
            done = paid_completion(self.campaign, self.journal, self.sheet, letter="F", stage="diagnostic",
                                   binding=binding(), messages=messages(), response_format=None, transport=transport,
                                   bound=self.bound, clock=self.clock, precount=self.counter(fake))
            self.assertEqual(done.outcome, "ok")
            self.assertEqual(len(fake.count_log), 1)
        self.assertEqual(self.campaign.ledger.totals()["request_count"], 1)

    def test_strict_parse(self):
        self.assertEqual(parse_input_tokens(b'{"object": "response.input_tokens", "input_tokens": 7}'), 7)
        for body in (b"nope", b"[]", b'{"input_tokens": 7}', b'{"object": "response.input_tokens", "input_tokens": -1}',
                     b'{"object": "response.input_tokens", "input_tokens": "7"}',
                     b'{"object": "response", "input_tokens": 7}'):
            with self.subTest(body=body), self.assertRaises(WireError):
                parse_input_tokens(body)

    def test_http_error_and_bad_wire_are_wire_errors(self):
        with FakeOpenAI({"default": "ok"}, model_by_role={"default": "gpt-6-luna"}, token=KEY) as fake:
            counter = self.counter(fake)
            with self.assertRaises(WireError):
                counter({"model": 5, "messages": [{"role": "user", "content": "x"}]})  # fake answers 400
            with self.assertRaises(WireError):
                counter({"messages": []})
            os.environ.pop("PAID_TEST_KEY")
            with self.assertRaises(WireError):
                counter(build_wire(self.campaign.profile("diagnostic"), messages()))
        self.assertEqual(load_counts(self.campaign.directory), [])

    def test_host_rules(self):
        with self.assertRaises(WireError):
            InputTokenCounter(self.campaign, self.campaign.directory, "http://api.openai.com/v1", "K", context={})
        with self.assertRaises(WireError):
            InputTokenCounter(self.campaign, self.campaign.directory, "https://evil.example/v1", "K", context={})
        counter = InputTokenCounter(self.campaign, self.campaign.directory, "http://127.0.0.1:9/v1", "PAID_TEST_KEY",
                                    context={})
        with self.assertRaises(WireError):  # connection refused = WireError before any reservation
            counter(build_wire(self.campaign.profile("diagnostic"), messages()))
        self.assertEqual(self.campaign.ledger.totals()["request_count"], 0)

    def test_record_conflict_is_a_stop(self):
        with FakeOpenAI({"default": "ok"}, model_by_role={"default": "gpt-6-luna"}, token=KEY) as fake:
            counter = self.counter(fake)
            wire = build_wire(self.campaign.profile("diagnostic"), messages())
            counter(wire)
            path = next((Path(self.campaign.directory) / "counts").glob("*.json"))
            record = json.loads(path.read_text())
            record["input_tokens"] += 1
            path.write_text(json.dumps(record))
            with self.assertRaises(WireError):
                counter(wire)


if __name__ == "__main__":
    unittest.main()
