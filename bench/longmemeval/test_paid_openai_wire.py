from decimal import Decimal
import itertools
import json
import os
from pathlib import Path
import socket
import tempfile
import unittest

from budget import BudgetExceeded
from paid_call_journal import CallJournal, JournalError, ModelDrift
from paid_campaign import CampaignError, SublimitExceeded, open_campaign
from paid_fake_openai import OUTPUT_LIMIT_400, FakeOpenAI
from paid_openai_wire import (BoundBreach, Rejected4xx, RunStopped, Transport, TransportError, WireError,
                              actual_cost, build_wire, check_wire, is_output_limit_400, paid_completion,
                              parse_completion)
from paid_price_sheet import PriceUnknown, StageBound
from paid_test_support import (SOURCES_BYTES_MAX, FakeTransport, binding, load_fixture_sheet, make_campaign,
                               messages, ok_body)

PROFILE = {"model": "gpt-6-luna", "reasoning_effort": "low", "max_completion_tokens": 1024}


def ok(body=None):
    return (200, {"x-request-id": "prov-1"}, body if body is not None else ok_body())


# Byte-for-byte the body OpenAI sent for holdout r2's selection request (HTTP 400, 2026-11-24).
OUTPUT_LIMIT_BODY = json.dumps(OUTPUT_LIMIT_400, indent=2).encode("utf-8")


def output_limit(body=OUTPUT_LIMIT_BODY, status=400):
    return (status, {"x-request-id": "prov-400"}, body)


def without_usage():
    body = json.loads(ok_body())
    del body["usage"]
    return json.dumps(body).encode("utf-8")


class BuildWireTests(unittest.TestCase):
    def test_shape_and_deep_copy(self):
        fmt = {"type": "json_schema", "json_schema": {"name": "x", "schema": {"type": "object"}}}
        wire = build_wire(PROFILE, messages(), fmt)
        self.assertEqual(wire["max_completion_tokens"], 1024)
        self.assertEqual(wire["reasoning_effort"], "low")
        self.assertEqual((wire["service_tier"], wire["stream"], wire["store"]), ("default", False, False))
        self.assertFalse({"temperature", "max_tokens", "seed"} & set(wire))
        fmt["json_schema"]["schema"]["type"] = "array"
        self.assertEqual(wire["response_format"]["json_schema"]["schema"], {"type": "object"})

    def test_refusals(self):
        cases = {"extra key": ({**PROFILE, "seed": 1}, messages()),
                 "missing key": ({k: v for k, v in PROFILE.items() if k != "model"}, messages()),
                 "bad effort": ({**PROFILE, "reasoning_effort": "ultra"}, messages()),
                 "three messages": (PROFILE, messages() + [{"role": "user", "content": "x"}]),
                 "wrong roles": (PROFILE, list(reversed(messages())))}
        for label, (profile, msgs) in cases.items():
            with self.subTest(case=label), self.assertRaises(WireError):
                build_wire(profile, msgs)


class CheckWireTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.sheet = load_fixture_sheet(self.directory.name)
        self.bound = self.sheet.stage_profile("diagnostic", SOURCES_BYTES_MAX, True)
        self.wire = build_wire(PROFILE, messages())

    def test_wire_refusals(self):
        self.assertEqual(self.bound.r_tokens, 44608)
        check_wire(self.wire, self.bound, self.sheet, "p1")
        cases = {"legacy max_tokens": {"max_tokens": 5}, "wrong model": {"model": "gpt-6.2-astra"},
                 "wrong O_i": {"max_completion_tokens": 512}, "store true": {"store": True},
                 "oversized": {"messages": messages(user="x" * 50_000)}}
        for label, change in cases.items():
            with self.subTest(case=label), self.assertRaises(WireError):
                check_wire({**self.wire, **change}, self.bound, self.sheet, "p1")
        with self.assertRaises(WireError):
            check_wire(self.wire, self.bound, self.sheet, "p3")

    def test_policies(self):
        astra = build_wire({**PROFILE, "model": "gpt-6.2-astra", "max_completion_tokens": 16000}, messages())
        with self.assertRaises(PriceUnknown):
            check_wire(astra, StageBound("judge", "gpt-6.2-astra", 300_000, 16000), self.sheet, "p1")
        sol = build_wire({**PROFILE, "model": "gpt-6.1-sol"}, messages())
        with self.assertRaises(PriceUnknown):
            check_wire(sol, StageBound("diagnostic", "gpt-6.1-sol", 1000, 1024), self.sheet, "p2")
        check_wire(self.wire, self.bound, self.sheet, "p2")


class TransportTests(unittest.TestCase):
    def test_construction_guards(self):
        with self.assertRaises(WireError):
            Transport("https://api.openai.com/v1", "K", {"127.0.0.1"})
        with self.assertRaises(WireError):
            Transport("http://api.openai.com/v1", "K", {"api.openai.com"})
        transport = Transport("http://127.0.0.1:1/v1", "PAID_TEST_MISSING_KEY", {"127.0.0.1"})
        self.assertEqual(transport.host, "127.0.0.1")
        os.environ.pop("PAID_TEST_MISSING_KEY", None)
        with self.assertRaises(WireError):
            transport(b"{}", 1)


class ParseAndCostTests(unittest.TestCase):
    def test_parse(self):
        parsed = parse_completion(ok_body())
        self.assertEqual(parsed["outcome"], "ok")
        self.assertEqual(parsed["usage"], {"prompt_tokens": 100, "completion_tokens": 10, "total_tokens": 110,
                                           "reasoning_tokens": None, "cached_tokens": None})
        self.assertEqual(parse_completion(ok_body(finish="length"))["outcome"], "length")
        bad = {"no usage": without_usage(), "no model": ok_body(model=""), "not json": b"nope",
               "inconsistent": ok_body(usage={"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 5}),
               "two choices": ok_body(choices=[json.loads(ok_body())["choices"][0]] * 2),
               "duplicate keys": b'{"a":1,"a":2}'}
        for label, body in bad.items():
            with self.subTest(case=label), self.assertRaises(WireError):
                parse_completion(body)

    def test_actual_cost(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        sheet = load_fixture_sheet(directory.name)
        usage = {"prompt_tokens": 100, "completion_tokens": 10}
        self.assertEqual(actual_cost(sheet, "gpt-6-luna", usage, "1"), Decimal("0.000015"))
        big = {"prompt_tokens": 300_000, "completion_tokens": 10}
        self.assertEqual(actual_cost(sheet, "gpt-6.2-astra", big, "1"), Decimal("4.8006"))
        self.assertEqual(actual_cost(sheet, "gpt-6.1-sol", big, "7.5"), Decimal("7.5"))


class OutputLimitRuleTests(unittest.TestCase):
    def test_only_the_output_limit_400_matches(self):
        error = OUTPUT_LIMIT_400["error"]
        self.assertTrue(is_output_limit_400(400, error))
        self.assertTrue(is_output_limit_400(400, json.loads(OUTPUT_LIMIT_BODY)["error"]))  # the r2 evidence body
        cases = {"429": (429, error), "401": (401, error), "500": (500, error), "no error object": (400, None),
                 "other type": (400, {**error, "type": "rate_limit_error"}),
                 "other message": (400, {**error, "message": "Invalid schema for response_format"}),
                 "only max_tokens": (400, {**error, "message": "max_tokens is too large"}),
                 "param set": (400, {**error, "param": "max_tokens"}),
                 "code set": (400, {**error, "code": "invalid_value"}),
                 "parts without the prefix": (400, {**error, "message": "Invalid max_tokens; model output limit is 4096"}),
                 "message not a string": (400, {**error, "message": None})}
        for label, (status, err) in cases.items():
            with self.subTest(case=label):
                self.assertFalse(is_output_limit_400(status, err))


class PaidCompletionTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.sheet = load_fixture_sheet(self.directory.name)
        self.bound = self.sheet.stage_profile("diagnostic", SOURCES_BYTES_MAX, True)
        self.clock = itertools.count(1_700_000_000).__next__
        os.environ["PAID_TEST_KEY"] = "fixture-only-key"
        self.addCleanup(os.environ.pop, "PAID_TEST_KEY", None)
        self.open(make_campaign(self.directory.name, self.sheet))

    def open(self, campaign):
        self.campaign = campaign
        self.journal = CallJournal(campaign.directory)
        self.calls = Path(campaign.directory) / "calls"
        return campaign

    def call(self, transport, letter="F", **kw):
        args = {"binding": binding(), "messages": messages(), "response_format": None, "acknowledged": frozenset()}
        args.update(kw)
        return paid_completion(self.campaign, self.journal, self.sheet, letter=letter, stage="diagnostic",
                               transport=transport, bound=self.bound, clock=self.clock, **args)

    def files(self, rid):
        return sorted(p.name.split(".")[1] for p in self.calls.glob(f"{rid}.*.json"))

    def settled(self, rid):
        return json.loads((self.calls / f"{rid}.settled.json").read_text())

    def test_reservation_before_http(self):
        transport = FakeTransport([ok()], journal_dir=self.campaign.directory)
        done = self.call(transport)
        # At call time the reservation is already durable in budget.json and sent.json is on disk.
        self.assertEqual((transport.saw_reserved, transport.saw_sent), ([True], [True]))
        totals = self.campaign.ledger.totals()
        self.assertEqual((totals["request_count"], totals["pending_count"], totals["spent_usd"]),
                         (1, 0, Decimal("0.000015")))
        self.assertEqual(self.files(done.request_id), ["intent", "request", "response", "sent", "settled"])
        self.assertEqual((done.outcome, done.replayed, done.provider_request_id), ("ok", False, "prov-1"))

    def test_budget_exhausted_means_zero_transports(self):
        tiny = {**{letter: "0.0000002" for letter in "EFSAR"}, "K": "0"}
        self.open(make_campaign(Path(self.directory.name) / "tiny", self.sheet, limit_usd="0.000001",
                                sublimits_usd=tiny))
        transport = FakeTransport([ok()])
        with self.assertRaises((BudgetExceeded, SublimitExceeded)):
            self.call(transport)
        self.assertEqual(transport.calls, [])
        self.assertEqual(list(self.calls.glob("*.intent.json")), [])

    def test_sublimit_stops_before_reserve(self):
        sub = {"E": "10", "F": "0.000001", "S": "30", "A": "10", "R": "10", "K": "0.2374"}
        self.open(make_campaign(Path(self.directory.name) / "sub", self.sheet, sublimits_usd=sub))
        transport = FakeTransport([ok()])
        with self.assertRaises(SublimitExceeded):
            self.call(transport)
        self.assertEqual(transport.calls, [])
        self.assertEqual(self.campaign.ledger.totals()["request_count"], 0)

    def test_length_is_settled(self):
        done = self.call(FakeTransport([ok(ok_body(finish="length"))]))
        self.assertEqual(done.outcome, "length")
        self.assertEqual(self.settled(done.request_id)["outcome"], "length")
        totals = self.campaign.ledger.totals()
        self.assertEqual((totals["pending_count"], totals["spent_usd"] > 0), (0, True))

    def test_missing_usage_is_uncertain_and_blocks(self):
        transport = FakeTransport([ok(without_usage()), ok()])
        with self.assertRaises(RunStopped):
            self.call(transport)
        rid = next(iter(self.journal.state.uncertain))
        self.assertTrue((self.calls / f"{rid}.uncertain.json").exists())
        self.assertEqual(self.campaign.ledger.totals()["pending_count"], 1)
        with self.assertRaises(RunStopped):
            self.call(transport, binding=binding(qid="q2"))
        self.assertEqual(len(transport.calls), 1)
        done = self.call(transport, binding=binding(qid="q2"), acknowledged={rid})
        self.assertEqual((done.outcome, len(transport.calls)), ("ok", 2))

    def test_transport_failures_are_uncertain(self):
        failures = (TransportError("timeout"), socket.timeout("t"), (500, {}, b'{"error": {"message": "x"}}'))
        for index, exc in enumerate(failures):
            with self.subTest(failure=exc):
                self.open(make_campaign(Path(self.directory.name) / f"fail{index}", self.sheet))
                with self.assertRaises(RunStopped):
                    self.call(FakeTransport([exc]))
                self.assertEqual(len(self.journal.state.uncertain), 1)
                self.assertEqual(self.campaign.ledger.totals()["pending_count"], 1)

    def test_rejected_4xx_costs_nothing(self):
        with self.assertRaises(Rejected4xx):
            self.call(FakeTransport([(400, {}, b'{"error": {"message": "rejected", "type": "invalid_request_error"}}')]))
        rid = next(iter(self.journal.state.settled))
        record = self.settled(rid)
        self.assertEqual((record["outcome"], record["actual_usd"]), ("rejected_4xx", "0"))
        totals = self.campaign.ledger.totals()
        self.assertEqual((totals["spent_usd"], totals["pending_count"]), (Decimal(0), 0))

    def test_output_limit_400_settles_as_length_at_zero(self):
        transport = FakeTransport([output_limit(), ok()])
        done = self.call(transport)
        self.assertEqual((done.outcome, done.usage, done.model, done.finish_reason, done.text, done.actual_usd,
                          done.provider_request_id, done.replayed), ("length", None, None, None, "", "0", "prov-400", False))
        record = self.settled(done.request_id)
        self.assertEqual({k: record[k] for k in ("http_status", "outcome", "model_observed", "usage", "finish_reason",
                                                 "actual_usd")},
                         {"http_status": 400, "outcome": "length", "model_observed": None, "usage": None,
                          "finish_reason": None, "actual_usd": "0"})
        totals = self.campaign.ledger.totals()
        self.assertEqual((totals["spent_usd"], totals["pending_count"], totals["request_count"]), (Decimal(0), 0, 1))
        self.assertEqual(list((Path(self.campaign.directory) / "pins").iterdir()), [])  # no model observed, none pinned
        self.assertEqual(self.call(transport, binding=binding(qid="q2")).outcome, "ok", "no stop, no block")

    def test_other_4xx_and_non_json_400_are_unchanged(self):
        error = OUTPUT_LIMIT_400["error"]
        rejected = {"other message": (400, {**error, "message": "rejected"}),
                    "other type": (400, {**error, "type": "invalid_json_schema"}),
                    "429": (429, error), "401": (401, {"message": "bad key", "type": "invalid_request_error"})}
        for index, (label, (status, err)) in enumerate(rejected.items()):
            with self.subTest(case=label):
                self.open(make_campaign(Path(self.directory.name) / f"r{index}", self.sheet))
                with self.assertRaises(Rejected4xx):
                    self.call(FakeTransport([output_limit(json.dumps({"error": err}).encode("utf-8"), status)]))
                [record] = self.journal.state.settled.values()
                self.assertEqual((record["outcome"], record["actual_usd"]), ("rejected_4xx", "0"))
        self.open(make_campaign(Path(self.directory.name) / "nonjson", self.sheet))
        with self.assertRaises(RunStopped):
            self.call(FakeTransport([output_limit(b"max_tokens or model output limit")]))
        self.assertEqual((len(self.journal.state.uncertain), len(self.journal.state.settled)), (1, 0))

    def test_output_limit_400_replays_as_length_after_restart(self):
        transport = FakeTransport([output_limit()])
        first = self.call(transport)
        self.open(open_campaign(self.campaign.directory, self.sheet))  # process restart: re-read from disk
        again = self.call(transport)
        self.assertEqual((again.request_id, again.replayed, again.outcome, again.usage, again.model, again.text),
                         (first.request_id, True, "length", None, None, ""))
        self.assertEqual((len(transport.calls), self.campaign.ledger.totals()["request_count"]), (1, 1))

    def test_tampered_stored_output_limit_body_is_not_replayed(self):
        transport = FakeTransport([output_limit()])
        first = self.call(transport)
        path = self.calls / f"{first.request_id}.response.json"
        stored = json.loads(path.read_text())
        stored["body_text"] = json.dumps({"error": {**OUTPUT_LIMIT_400["error"], "message": "rejected"}})
        path.write_text(json.dumps(stored))
        self.open(open_campaign(self.campaign.directory, self.sheet))
        with self.assertRaisesRegex(RunStopped, "no longer matches"):
            self.call(transport)
        self.assertEqual(len(transport.calls), 1)

    def test_replayed_rejected_4xx_still_stops(self):
        transport = FakeTransport([(400, {}, b'{"error": {"message": "rejected", "type": "invalid_request_error"}}')])
        with self.assertRaises(Rejected4xx):
            self.call(transport)
        self.open(open_campaign(self.campaign.directory, self.sheet))
        with self.assertRaisesRegex(RunStopped, "rejected_4xx"):
            self.call(transport)
        self.assertEqual(len(transport.calls), 1)

    def test_bound_breaches(self):
        with self.assertRaises(BoundBreach):
            self.call(FakeTransport([ok(ok_body(prompt=10 ** 6))]))
        totals = self.campaign.ledger.totals()  # 10**6 prompt bills at luna's long tier: 0.2 + 10 * 0.75e-6
        self.assertEqual((totals["pending_count"], totals["breached"], totals["spent_usd"]),
                         (0, True, Decimal("0.2000075")))
        self.open(make_campaign(Path(self.directory.name) / "o", self.sheet))
        with self.assertRaises(BoundBreach):
            self.call(FakeTransport([ok(ok_body(completion=2000))]))
        totals = self.campaign.ledger.totals()
        self.assertEqual((totals["pending_count"], totals["breached"], totals["spent_usd"]),
                         (0, False, Decimal("0.00101")))

    def test_model_pin_and_drift(self):
        first = self.call(FakeTransport([ok(ok_body(model="gpt-6-luna-2026-01-01"))]))
        pin = json.loads((Path(self.campaign.directory) / "pins" / "gpt-6-luna.json").read_text())
        self.assertEqual((pin["model"], pin["first_request_id"]), ("gpt-6-luna-2026-01-01", first.request_id))
        spent = self.campaign.ledger.totals()["spent_usd"]
        with self.assertRaises(ModelDrift):
            self.call(FakeTransport([ok(ok_body(model="gpt-6-luna-2026-02-02"))]), binding=binding(qid="q2"))
        drifted = [r for r in self.journal.state.settled if r != first.request_id][0]
        self.assertEqual(self.settled(drifted)["outcome"], "model_drift")
        totals = self.campaign.ledger.totals()
        self.assertEqual((totals["pending_count"], totals["spent_usd"] > spent), (0, True))

    def test_loopback_fake_provider(self):
        with FakeOpenAI({"default": ["ok", "model_drift"]}, model_by_role={"default": "gpt-6-luna"}) as fake:
            transport = Transport(fake.url, "PAID_TEST_KEY", {"127.0.0.1"})
            done = self.call(transport)
            self.assertEqual((done.outcome, done.model), ("ok", "gpt-6-luna"))
            with self.assertRaises(ModelDrift):
                self.call(transport, binding=binding(qid="q2"))
        wire = fake.log[0]["wire"]
        self.assertEqual((wire["max_completion_tokens"], wire["reasoning_effort"], wire["store"]), (1024, "low", False))
        self.assertFalse({"max_tokens", "temperature"} & set(wire))
        self.assertEqual(len(fake.log), 2)

    def test_replay_of_identical_input(self):
        transport = FakeTransport([ok()])
        first = self.call(transport)
        again = self.call(transport)
        self.assertEqual((again.request_id, again.replayed, again.text), (first.request_id, True, first.text))
        self.assertEqual(len(transport.calls), 1)
        self.assertEqual(self.campaign.ledger.totals()["request_count"], 1)

    def nothing_reserved(self, transport):
        self.assertEqual((transport.calls, self.campaign.ledger.totals()["request_count"],
                          sorted(self.calls.glob("*.intent.json"))), ([], 0, []))

    def test_missing_credential_refuses_before_any_reservation(self):
        del os.environ["PAID_TEST_KEY"]
        transport = FakeTransport([ok()])
        with self.assertRaisesRegex(WireError, "missing credential"):
            self.call(transport)
        self.nothing_reserved(transport)

    def test_dry_run_campaign_admits_only_loopback_transports(self):
        self.open(make_campaign(self.directory.name + "/dry", self.sheet, mode="dry_run",
                                provider={"base_url": "https://api.openai.com/v1", "api_key_env": "PAID_TEST_KEY",
                                          "allowed_host": "api.openai.com"}))
        remote = FakeTransport([ok()], host="api.openai.com")
        with self.assertRaisesRegex(CampaignError, "dry_run"):
            self.call(remote)
        self.nothing_reserved(remote)
        self.assertEqual(self.call(FakeTransport([ok()])).outcome, "ok")  # loopback never costs money

    def test_transport_host_must_match_the_campaign_provider(self):
        self.open(make_campaign(self.directory.name + "/host", self.sheet,
                                provider={"base_url": "https://api.openai.com/v1", "api_key_env": "PAID_TEST_KEY",
                                          "allowed_host": "api.openai.com"}))
        other = FakeTransport([ok()], host="evil.example")
        with self.assertRaisesRegex(WireError, "allowed host"):
            self.call(other)
        self.nothing_reserved(other)
        self.assertEqual(Transport.from_campaign(self.campaign).host, "api.openai.com")
        with self.assertRaises(WireError):
            self.call(FakeTransport([ok()], host=""))

    def test_unknown_letter_or_stage_is_a_campaign_error(self):
        transport = FakeTransport([ok()])
        with self.assertRaises(CampaignError):
            self.call(transport, letter="X")
        with self.assertRaises(CampaignError):
            paid_completion(self.campaign, self.journal, self.sheet, letter="F", stage="nope", binding=binding(),
                            messages=messages(), response_format=None, transport=transport, bound=self.bound)
        self.nothing_reserved(transport)

    def test_redirect_is_uncertain(self):
        with FakeOpenAI({"default": "redirect"}, model_by_role={"default": "gpt-6-luna"}) as fake:
            transport = Transport(fake.url, "PAID_TEST_KEY", {"127.0.0.1"})
            with self.assertRaises(RunStopped):
                self.call(transport)
        [rid] = self.journal.state.uncertain
        self.assertEqual(json.loads((self.calls / f"{rid}.uncertain.json").read_text())["error_type"], "WireError")
        self.assertEqual(self.campaign.ledger.totals()["pending_count"], 1)

    def test_settled_journal_with_open_ledger_reservation_is_not_replayed(self):
        first = self.call(FakeTransport([ok()]))
        state = json.loads((self.campaign.directory / "budget.json").read_text())
        state["requests"][first.request_id]["actual_usd"] = None  # crash between journal.settled and ledger.settle
        (self.campaign.directory / "budget.json").write_text(json.dumps(state))
        with self.assertRaisesRegex(JournalError, "mismatch"):
            self.call(FakeTransport([ok()]))


if __name__ == "__main__":
    unittest.main()
