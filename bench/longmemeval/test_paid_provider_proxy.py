import json
import os
import tempfile
import unittest
from urllib.error import HTTPError
from urllib.request import ProxyHandler, Request, build_opener

from paid_call_journal import CallJournal, sha256_hex
from paid_fake_openai import FakeOpenAI
from paid_openai_wire import InputTokensBreach, Transport, parse_completion
from paid_provider_proxy import KEY_ENV, ProviderProxy, system_fingerprint, validate_extraction_body
from paid_test_support import PROFILES, FakeTransport, load_fixture_sheet, make_campaign, ok_body

PROFILE = PROFILES["extraction"]
MANIFEST = "a" * 64
SYSTEM = ("SYNTHETIC extraction SYSTEM\nRules.\n\nThe conversation_date is: 2025-07-10\n\n"
          "Return ONLY valid JSON.")
SYSTEM_DAY2 = SYSTEM.replace("2025-07-10", "2025-07-11")
NER_SYSTEM = "SYNTHETIC entity extraction SYSTEM"
USER = '[CHUNK id="0"]\nuser: Synthetic turn 0 of h1.\nassistant: Noted.\n[/CHUNK]'
FACTS = json.dumps({"facts": [{"content": "A synthetic fact.", "fact_type": "fact", "confidence": 0.9}]})


def body(system=SYSTEM, user=USER, **overrides):
    wire = {"model": "gpt-6-luna", "messages": [{"role": "system", "content": system}, {"role": "user", "content": user}],
            "max_completion_tokens": 16384, "reasoning_effort": "low"}
    wire.update(overrides)
    return wire


def reply(text=FACTS, **extra):
    return (200, {"x-request-id": "up-1"}, ok_body(text=text, **extra))


def post(proxy, wire, token=None, path="/chat/completions"):
    data = json.dumps(wire).encode("utf-8")
    request = Request(proxy.url + path, data=data, method="POST", headers={
        "Content-Type": "application/json", "Authorization": "Bearer " + (proxy.token if token is None else token)})
    opener = build_opener(ProxyHandler({}))
    try:
        with opener.open(request, timeout=10) as response:
            return response.status, json.loads(response.read())
    except HTTPError as error:
        with error:
            return error.code, json.loads(error.read())


class ValidateBodyTests(unittest.TestCase):
    def test_fingerprint_normalises_only_the_conversation_date_line(self):
        self.assertEqual(system_fingerprint(SYSTEM), system_fingerprint(SYSTEM_DAY2))
        self.assertEqual(system_fingerprint(SYSTEM), system_fingerprint(SYSTEM.replace("2025-07-10", "unknown")))
        self.assertNotEqual(system_fingerprint(SYSTEM), system_fingerprint(NER_SYSTEM))
        self.assertNotEqual(system_fingerprint(SYSTEM), system_fingerprint(SYSTEM + " "))

    def test_conforming_body_and_refusals(self):
        messages, fingerprint = validate_extraction_body(body(), PROFILE, None)
        self.assertEqual([m["role"] for m in messages], ["system", "user"])
        self.assertEqual(fingerprint, system_fingerprint(SYSTEM))
        self.assertEqual(validate_extraction_body(json.dumps(body(system=SYSTEM_DAY2)).encode(), PROFILE, fingerprint)[1],
                         fingerprint)
        cases = {"legacy max_tokens": body(max_tokens=16384), "legacy temperature": body(temperature=0.0),
                 "extra key": body(stream=False), "missing effort": {k: v for k, v in body().items() if k != "reasoning_effort"},
                 "wrong model": body(model="gpt-6.2-astra"), "wrong limit": body(max_completion_tokens=1024),
                 "bool limit": body(max_completion_tokens=True), "wrong effort": body(reasoning_effort="high"),
                 "three messages": body(messages=body()["messages"] + [{"role": "user", "content": "x"}]),
                 "roles swapped": body(messages=list(reversed(body()["messages"]))),
                 "content not text": body(user=None), "not an object": [body()], "not json": b"{",
                 "over cap": json.dumps(body(user="x" * 100)).encode()}
        for label, wire in cases.items():
            cap = 50 if label == "over cap" else 4 * 1024 * 1024
            with self.subTest(case=label), self.assertRaises(ValueError):
                validate_extraction_body(wire, PROFILE, None, body_cap=cap)
        with self.assertRaises(ValueError):
            validate_extraction_body(body(system=NER_SYSTEM), PROFILE, fingerprint)
        with self.assertRaises(ValueError):
            validate_extraction_body(body(), {"model": "gpt-6-luna"}, None)


class ProviderProxyTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.sheet = load_fixture_sheet(self.directory.name)
        self.campaign = make_campaign(self.directory.name, self.sheet)
        self.journal = CallJournal(self.campaign.directory)
        previous = os.environ.get("PAID_TEST_KEY")
        os.environ["PAID_TEST_KEY"] = "synthetic"
        self.addCleanup(lambda: os.environ.pop("PAID_TEST_KEY") if previous is None
                        else os.environ.__setitem__("PAID_TEST_KEY", previous))

    def transport(self, *replies):
        return FakeTransport(list(replies), journal_dir=self.campaign.directory)

    def proxy(self, transport, **kwargs):
        return ProviderProxy(self.campaign, self.journal, self.sheet, transport, extraction_profile=PROFILE,
                             manifest_sha256=MANIFEST, **kwargs)

    def test_conforming_request_is_reserved_before_upstream_and_journaled(self):
        transport = self.transport(reply())
        with self.proxy(transport) as proxy:
            self.assertEqual(proxy.key_env, KEY_ENV)
            self.assertTrue(proxy.url.startswith("http://127.0.0.1:"))
            proxy.bind("h1", "h1-s1", 5)
            status, answer = post(proxy, body())
            records = proxy.unbind()
        self.assertEqual(status, 200)
        self.assertEqual((transport.saw_reserved, transport.saw_sent), ([True], [True]))
        parsed = parse_completion(json.dumps(answer).encode("utf-8"))
        self.assertEqual((parsed["text"], parsed["finish_reason"], parsed["model"]), (FACTS, "stop", "gpt-6-luna"))
        upstream = json.loads(transport.calls[0][0])
        self.assertEqual((upstream["model"], upstream["max_completion_tokens"], upstream["reasoning_effort"]),
                         ("gpt-6-luna", 16384, "low"))
        self.assertFalse({"temperature", "max_tokens"} & set(upstream))
        self.assertEqual(upstream["messages"][1]["content"], USER)
        (rid, intent), = self.journal.state.intents.items()
        self.assertEqual((intent["letter"], intent["stage"]), ("E", "extraction"))
        binding = intent["binding"]
        self.assertEqual((binding["history_id"], binding["session_id"], binding["ordinal"], binding["attempt"]),
                         ("h1", "h1-s1", 0, 1))
        self.assertIsNone(binding["question_id"])
        self.assertEqual(binding["context_sha256"], sha256_hex(USER.encode()))
        self.assertEqual(binding["prompt_sha256"], system_fingerprint(SYSTEM))
        self.assertEqual(binding["manifest_sha256"], MANIFEST)
        self.assertEqual(self.journal.state.settled[rid]["outcome"], "ok")
        self.assertEqual(len(records), 1)
        self.assertEqual((records[0]["request_id"], records[0]["outcome"], records[0]["ordinal"]), (rid, "ok", 0))
        self.assertEqual(records[0]["usage"]["prompt_tokens"], 100)
        self.assertEqual(proxy.system_sha256, system_fingerprint(SYSTEM))
        self.assertFalse(proxy.halted)
        self.assertEqual(proxy.records, records)

    def test_non_conforming_requests_halt_before_any_reservation(self):
        cases = {"legacy max_tokens": body(max_tokens=16384), "legacy temperature": body(temperature=0.0),
                 "wrong model": body(model="gpt-6.2-astra"), "wrong limit": body(max_completion_tokens=1024),
                 "three messages": body(messages=body()["messages"] + [{"role": "user", "content": "x"}]),
                 "NER prompt": body(system=NER_SYSTEM)}
        for label, wire in cases.items():
            with self.subTest(case=label):
                transport = self.transport(reply())
                with self.proxy(transport, system_sha256=system_fingerprint(SYSTEM)) as proxy:
                    proxy.bind("h1", "h1-s1", 5)
                    status, answer = post(proxy, wire)
                    self.assertEqual(status, 400)
                    self.assertIn("error", answer)
                    self.assertTrue(proxy.halted)
                    self.assertIn("non-conforming", str(proxy.halt_reason))
                    status, _ = post(proxy, body())
                    self.assertEqual(status, 400, "after a halt every request is refused")
                    self.assertEqual([r["outcome"] for r in proxy.records], ["halted", "refused_after_halt"])
                self.assertEqual(transport.calls, [])
        self.assertEqual(self.journal.state.intents, {})
        self.assertEqual(self.campaign.ledger.totals()["request_count"], 0)

    def test_window_cap_no_window_and_concurrency_halt(self):
        transport = self.transport(reply(), reply())
        with self.proxy(transport) as proxy:
            proxy.bind("h1", "h1-s1", 1)
            self.assertEqual(post(proxy, body())[0], 200)
            status, _ = post(proxy, body())
            self.assertEqual(status, 400)
            self.assertTrue(proxy.halted)
            self.assertIn("beyond max_requests", str(proxy.halt_reason))
            self.assertEqual([r["ordinal"] for r in proxy.unbind()], [0, 1])
        with self.proxy(self.transport(reply())) as proxy:
            status, _ = post(proxy, body())
            self.assertEqual((status, proxy.halted), (400, True))
            self.assertIn("outside a bound session window", str(proxy.halt_reason))
            proxy.bind("h1", "h1-s1", 5)
            self.assertEqual(post(proxy, body())[0], 400)
        with self.proxy(self.transport(reply())) as proxy:
            proxy.bind("h1", "h1-s1", 5)
            with proxy._busy:  # another request is in flight
                status, _ = post(proxy, body())
            self.assertEqual((status, proxy.halted), (400, True))
            self.assertIn("concurrent", str(proxy.halt_reason))
        with self.proxy(self.transport(reply())) as proxy:
            proxy.bind("h1", "h1-s1", 5)
            self.assertEqual(post(proxy, body(), path="/embeddings")[0], 400)
            self.assertTrue(proxy.halted)
        self.assertEqual(len(self.journal.state.intents), 1)

    def test_exit_waits_for_the_request_in_flight(self):
        """Tearing the proxy down must not leave a `sent` request without its settlement."""
        import threading
        started, gate = threading.Event(), threading.Event()
        inner = self.transport(reply())

        class Blocking:
            host = "127.0.0.1"

            def __call__(self, body, timeout_s):
                started.set()
                gate.wait(5)
                return inner(body, timeout_s)

        proxy = self.proxy(Blocking())
        proxy.__enter__()
        proxy.bind("h1", "h1-s1", 5)
        outcome = {}
        poster = threading.Thread(target=lambda: outcome.update(status=post(proxy, body())[0]))
        poster.start()
        self.assertTrue(started.wait(5))
        closer = threading.Thread(target=proxy.__exit__, args=(None, None, None))
        closer.start()
        closer.join(0.2)
        self.assertTrue(closer.is_alive(), "__exit__ must wait for the request in flight")
        gate.set()
        poster.join(5)
        closer.join(5)
        self.assertFalse(closer.is_alive())
        self.assertEqual(outcome.get("status"), 200)
        self.assertEqual(len(self.journal.state.settled), 1)
        self.assertEqual(self.journal.state.unsettled_sent, [])

    def test_bad_content_length_is_400_and_halt(self):
        import http.client
        for value in ("abc", "-5"):
            with self.proxy(self.transport(reply())) as proxy:
                proxy.bind("h1", "h1-s1", 5)
                port = int(proxy.url.rsplit(":", 1)[1].split("/")[0])
                connection = http.client.HTTPConnection("127.0.0.1", port, timeout=5)
                try:
                    connection.request("POST", "/v1/chat/completions", body=b"{}",
                                       headers={"Content-Type": "application/json", "Content-Length": value,
                                                "Authorization": "Bearer " + proxy.token})
                    response = connection.getresponse()
                    status = response.status
                    response.read()
                finally:
                    connection.close()
                self.assertEqual((status, proxy.halted), (400, True), value)
                self.assertIn("Content-Length", str(proxy.halt_reason))
        self.assertEqual(len(self.journal.state.intents), 0)

    def test_bad_bearer_is_401_not_a_halt(self):
        transport = self.transport(reply())
        with self.proxy(transport) as proxy:
            proxy.bind("h1", "h1-s1", 5)
            status, answer = post(proxy, body(), token="wrong")
            self.assertEqual((status, proxy.halted), (401, False))
            self.assertEqual(answer["error"]["message"], "invalid token")
            self.assertEqual(post(proxy, body())[0], 200)
            self.assertEqual([r["outcome"] for r in proxy.unbind()], ["unauthorized", "ok"])
        self.assertEqual(transport.calls.__len__(), 1)

    def test_length_outcome_is_forwarded_and_settled(self):
        transport = self.transport(reply(text="", finish="length", completion=16384), reply())
        with self.proxy(transport) as proxy:
            proxy.bind("h1", "h1-s1", 5)
            status, answer = post(proxy, body())
            self.assertEqual(status, 200)
            self.assertEqual(answer["choices"][0]["finish_reason"], "length")
            self.assertFalse(proxy.halted)
            status, _ = post(proxy, body(user=USER[:40]))  # the engine's split retry
            self.assertEqual(status, 200)
            records = proxy.unbind()
        self.assertEqual([r["outcome"] for r in records], ["length", "ok"])
        self.assertEqual([r["ordinal"] for r in records], [0, 1])
        settled = self.journal.state.settled
        self.assertEqual(sorted(s["outcome"] for s in settled.values()), ["length", "ok"])
        self.assertEqual(self.campaign.ledger.totals()["request_count"], 2)

    def test_precount_is_consulted_and_an_over_bound_count_stops_without_reservation(self):
        counts = []

        def counting(wire):
            counts.append(wire["model"])
            return 10

        transport = self.transport(reply())
        with self.proxy(transport, precount=counting) as proxy:
            proxy.bind("h1", "h1-s1", 5)
            self.assertEqual(post(proxy, body())[0], 200)
        self.assertEqual(counts, ["gpt-6-luna"])
        transport = self.transport(reply())
        with self.proxy(transport, precount=lambda wire: 10 ** 7) as proxy:
            proxy.bind("h1", "h1-s2", 5)
            status, answer = post(proxy, body())
            self.assertEqual(status, 502)
            self.assertEqual(answer["error"]["type"], "InputTokensBreach")
            self.assertIsInstance(proxy.halt_reason, InputTokensBreach)
            self.assertEqual(proxy.records[-1]["outcome"], "halted")
        self.assertEqual(transport.calls, [])
        self.assertEqual(self.campaign.ledger.totals()["request_count"], 1)
        self.assertEqual(len(self.journal.state.intents), 1)

    def test_two_sessions_get_their_own_ordinals_and_request_ids(self):
        transport = self.transport(reply(), reply())
        with self.proxy(transport) as proxy:
            proxy.bind("h1", "h1-s1", 5)
            self.assertEqual(post(proxy, body())[0], 200)
            first = proxy.unbind()
            proxy.bind("h1", "h1-s2", 5)
            self.assertEqual(post(proxy, body(system=SYSTEM_DAY2))[0], 200)
            second = proxy.unbind()
            self.assertFalse(proxy.halted)
        self.assertEqual(([r["ordinal"] for r in first], [r["ordinal"] for r in second]), ([0], [0]))
        self.assertNotEqual(first[0]["request_id"], second[0]["request_id"])
        self.assertEqual({r["session_id"] for r in first + second}, {"h1-s1", "h1-s2"})
        self.assertEqual(self.campaign.ledger.totals()["request_count"], 2)
        self.assertEqual(len(proxy.records), 2)

    def test_fingerprint_frozen_on_first_request_then_other_prompts_halt(self):
        transport = self.transport(reply(), reply())
        with self.proxy(transport) as proxy:
            self.assertIsNone(proxy.system_sha256)
            proxy.bind("h1", "h1-s1", 5)
            self.assertEqual(post(proxy, body())[0], 200)
            self.assertEqual(proxy.system_sha256, system_fingerprint(SYSTEM))
            status, _ = post(proxy, body(system=NER_SYSTEM))
            self.assertEqual((status, proxy.halted), (400, True))
            self.assertEqual(proxy.system_sha256, system_fingerprint(SYSTEM))
        self.assertEqual(len(transport.calls), 1)

    def test_fake_openai_upstream_through_the_real_transport(self):
        with FakeOpenAI({"extraction": "ok"}, model_by_role={"extraction": "gpt-6-luna"}) as fake:
            transport = Transport(fake.url, "PAID_TEST_KEY", {"127.0.0.1"})
            with self.proxy(transport) as proxy:
                proxy.bind("h1", "h1-s1", 5)
                status, answer = post(proxy, body())
                records = proxy.unbind()
            self.assertEqual(status, 200)
            self.assertEqual(json.loads(answer["choices"][0]["message"]["content"])["facts"][0]["fact_type"], "fact")
            self.assertEqual([entry["role"] for entry in fake.log], ["extraction"])
        self.assertEqual(records[0]["outcome"], "ok")
        self.assertEqual(self.journal.state.pins["gpt-6-luna"]["model"], "gpt-6-luna")


if __name__ == "__main__":
    unittest.main()
