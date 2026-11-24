import hashlib
import json
import os
import unittest
from urllib.error import HTTPError
from urllib.request import Request, urlopen

from paid_fake_openai import OUTPUT_LIMIT_400, SCENARIOS, FakeOpenAI, content_for
from paid_openai_wire import Transport, TransportError, WireError, build_wire, parse_completion
from paid_test_support import PROFILES

LUNA = {"default": "gpt-6-luna"}


def sha(text):
    return hashlib.sha256(text.encode("utf-8")).hexdigest()


def wire_for(system="S", user=None, profile="diagnostic"):
    content = json.dumps(user if user is not None else {"question": "q", "facts": []})
    return build_wire(PROFILES[profile], [{"role": "system", "content": system}, {"role": "user", "content": content}])


class FakeOpenAITests(unittest.TestCase):
    def setUp(self):
        os.environ["PAID_TEST_KEY"] = "t"
        self.addCleanup(os.environ.pop, "PAID_TEST_KEY", None)

    def post(self, fake, wire=None, timeout=5):
        transport = Transport(fake.url, "PAID_TEST_KEY", {"127.0.0.1"})
        return transport(json.dumps(wire or wire_for()).encode("utf-8"), timeout)

    def run_scenario(self, scenario, **kwargs):
        kwargs.setdefault("model_by_role", LUNA)
        with FakeOpenAI({"default": scenario}, token="t", **kwargs) as fake:
            self.assertTrue(fake.url.startswith("http://127.0.0.1:"))
            return self.post(fake)

    def test_every_scenario_is_covered(self):
        self.assertEqual(set(SCENARIOS), {"ok", "empty_facts", "length", "http500", "timeout", "model_drift",
                                          "missing_usage", "malformed_json", "bad_request_400", "usage_over_bound",
                                          "redirect", "capped", "capped_failed_attempt", "length_http400"})

    def test_length_http400_only_for_strict_json_schema(self):
        schema = {"type": "json_schema", "json_schema": {"name": "x", "strict": True, "schema": {"type": "object"}}}
        strict = build_wire(PROFILES["diagnostic"], [{"role": "system", "content": "S"}, {"role": "user", "content": "{}"}],
                            schema)
        with FakeOpenAI({"default": "length_http400"}, model_by_role=LUNA, token="t") as fake:
            status, _, body = self.post(fake, strict)
            self.assertEqual((status, json.loads(body)), (400, OUTPUT_LIMIT_400))
            status, _, body = self.post(fake)  # no response_format: OpenAI's ordinary 200 + length
            self.assertEqual((status, parse_completion(body)["outcome"]), (200, "length"))

    def test_capped_extraction_answers_distinct_facts(self):
        capped = json.loads(content_for("extraction", "", "capped"))
        self.assertEqual(len({f["content"] for f in capped["facts"]}), 22)
        self.assertNotIn("_fake_outcome", capped)
        failed = json.loads(content_for("extraction", "", "capped_failed_attempt"))
        self.assertEqual((failed["facts"], failed["_fake_outcome"]), (capped["facts"], {"failed_attempts": 1}))
        self.assertEqual(json.loads(content_for("extraction", "", "ok"))["facts"][0]["content"], "A synthetic fact.")

    def test_ok(self):
        status, headers, body = self.run_scenario("ok")
        done = parse_completion(body)
        self.assertEqual((status, done["finish_reason"], done["model"]), (200, "stop", "gpt-6-luna"))
        self.assertIn("x-request-id", {k.lower() for k in headers})

    def test_empty_facts_for_extraction_role(self):
        with FakeOpenAI({"extraction": "empty_facts"}, model_by_role={"extraction": "gpt-6-luna"}, token="t") as fake:
            status, _, body = self.post(fake, wire_for(profile="extraction"))
        self.assertEqual(status, 200)
        self.assertEqual(json.loads(parse_completion(body)["text"]), {"facts": []})
        self.assertEqual(fake.log[0]["role"], "extraction")

    def test_length(self):
        status, _, body = self.run_scenario("length")
        done = parse_completion(body)
        self.assertEqual((status, done["finish_reason"], done["text"]), (200, "length", ""))
        self.assertEqual(done["usage"]["completion_tokens"], PROFILES["diagnostic"]["max_completion_tokens"])

    def test_http500_and_bad_request_400(self):
        for scenario, code in (("http500", 500), ("bad_request_400", 400)):
            with self.subTest(scenario=scenario):
                status, _, body = self.run_scenario(scenario)
                self.assertEqual(status, code)
                self.assertIn("message", json.loads(body)["error"])

    def test_timeout_raises_transport_error(self):
        with self.assertRaises(TransportError):
            self.run_scenario("timeout")

    def test_redirect_is_refused_by_the_transport(self):
        with self.assertRaises(WireError):  # _NoRedirect: a 302 is never followed
            self.run_scenario("redirect")

    def test_model_drift(self):
        _, _, body = self.run_scenario("model_drift")
        self.assertTrue(parse_completion(body)["model"].endswith("-drifted"))

    def test_missing_usage_and_malformed_json_fail_parse(self):
        for scenario in ("missing_usage", "malformed_json"):
            with self.subTest(scenario=scenario):
                status, _, body = self.run_scenario(scenario)
                self.assertEqual(status, 200)
                with self.assertRaises(WireError):
                    parse_completion(body)

    def test_usage_over_bound(self):
        _, _, body = self.run_scenario("usage_over_bound")
        self.assertEqual(parse_completion(body)["usage"]["prompt_tokens"], 10 ** 7)

    def test_wrong_token_and_wrong_path(self):
        with FakeOpenAI({"default": "ok"}, model_by_role=LUNA, token="t") as fake:
            os.environ["PAID_TEST_KEY"] = "wrong"
            status, _, body = self.post(fake)
            self.assertEqual((status, "message" in json.loads(body)["error"]), (401, True))
            request = Request(fake.url + "/other", data=b"{}", method="POST",
                              headers={"Authorization": "Bearer t", "Content-Type": "application/json"})
            with self.assertRaises(HTTPError) as caught:
                urlopen(request, timeout=5)
            with caught.exception:
                self.assertEqual(caught.exception.code, 404)
            self.assertEqual(fake.log, [])

    def test_legacy_or_incomplete_wire_is_rejected_unlogged(self):
        legacy = wire_for()
        legacy["max_tokens"] = legacy.pop("max_completion_tokens")
        no_effort = wire_for()
        del no_effort["reasoning_effort"]
        with FakeOpenAI({"default": "ok"}, model_by_role=LUNA, token="t") as fake:
            for wire in (legacy, no_effort):
                status, _, body = self.post(fake, wire)
                self.assertEqual((status, "message" in json.loads(body)["error"]), (400, True))
        self.assertEqual(fake.log, [])

    def test_role_by_system_sha_and_stage_shapes(self):
        roles = {sha(r): r for r in ("judge", "diagnostic", "audit", "selection", "answer")}
        facts = [{"record_id": f"f{i}", "content": "x"} for i in range(3)]
        records = [{"record_id": f"r{i}"} for i in range(20)]
        with FakeOpenAI({"default": "ok"}, model_by_role=LUNA, role_by_system_sha256=roles, token="t") as fake:
            def ask(role, user, profile="diagnostic"):
                _, _, body = self.post(fake, wire_for(role, user, profile))
                self.assertEqual(fake.log[-1]["role"], role)
                return parse_completion(body)["text"]

            verdict = json.loads(ask("judge", {"candidate": "a long candidate answer text", "question": "q"}, "judge"))
            self.assertEqual((verdict["status"], verdict["answer_span"]), ("scored", "a long candidate answ"[:20]))
            self.assertTrue("a long candidate answer text".startswith(verdict["answer_span"]))
            diag = json.loads(ask("diagnostic", {"facts": facts, "question": "q"}))
            self.assertEqual(set(diag), {"answer", "coverage", "missing_parts", "conflict_fact_ids", "supporting_fact_ids"})
            self.assertEqual(diag["supporting_fact_ids"], ["f0"])
            audit = json.loads(ask("audit", {"facts": facts, "proposed_answer": "a", "question": "q"}, "audit"))
            self.assertEqual(set(audit), {"claims_entailed", "requested_scope_covered", "internally_consistent",
                                          "freshness_supported", "issues"})
            self.assertTrue(all(audit[k] is True for k in audit if k != "issues"))
            selection = json.loads(ask("selection", {"original_records": records, "facts": [], "question": "q"}, "selection"))
            self.assertEqual(selection["selected_ids"], [f"r{i}" for i in range(16)])
            self.assertEqual(ask("answer", {"evidence": {}, "question": "q"}, "answer"), "Synthetic answer.")
        self.assertEqual(json.loads(content_for("judge", json.dumps({"candidate": "abc"}), "ok"))["answer_span"], "abc")

    def test_role_by_normalised_extraction_fingerprint(self):
        from paid_provider_proxy import system_fingerprint
        prompt = "Extract facts.\nThe conversation_date is: {date}\nReturn JSON."
        normalised = system_fingerprint(prompt.format(date="2025-07-10"))
        self.assertEqual(normalised, system_fingerprint(prompt.format(date="2025-07-11")))
        with FakeOpenAI({"extraction": "empty_facts"}, model_by_role={"a": "gpt-6-luna", "b": "gpt-6-luna"},
                        role_by_system_sha256={normalised: "extraction"}, token="t") as fake:
            for day in ("2025-07-10", "2025-07-11"):
                status, _, body = self.post(fake, wire_for(system=prompt.format(date=day), profile="extraction"))
                self.assertEqual(status, 200)
                self.assertEqual(json.loads(parse_completion(body)["text"]), {"facts": []})
            self.assertEqual([entry["role"] for entry in fake.log], ["extraction", "extraction"])

    def test_usage_is_deterministic_and_bounded_by_bytes(self):
        wire = wire_for()
        with FakeOpenAI({"default": "ok"}, model_by_role=LUNA, token="t") as fake:
            first = parse_completion(self.post(fake, wire)[2])["usage"]
            second = parse_completion(self.post(fake, wire)[2])["usage"]
        self.assertEqual(first, second)
        size = sum(len(m["content"].encode("utf-8")) for m in wire["messages"])
        self.assertLessEqual(first["prompt_tokens"], size)

    def test_fifo_scenarios_and_constructor_validation(self):
        with FakeOpenAI({"default": ["ok", "length"]}, model_by_role=LUNA, token="t") as fake:
            finishes = [parse_completion(self.post(fake)[2])["finish_reason"] for _ in range(3)]
        self.assertEqual(finishes, ["stop", "length", "length"])
        self.assertEqual([e["scenario"] for e in fake.log], ["ok", "length", "length"])
        for bad in ({"default": "nope"}, {"default": []}, {"default": ["ok", "nope"]}):
            with self.subTest(bad=bad), self.assertRaises(ValueError):
                FakeOpenAI(bad, model_by_role=LUNA)

    def test_log_entries(self):
        wire = wire_for()
        raw = json.dumps(wire).encode("utf-8")
        with FakeOpenAI({"default": "ok"}, model_by_role=LUNA, token="t") as fake:
            self.post(fake, wire)
        entry = fake.log[0]
        self.assertEqual(set(entry), {"role", "scenario", "wire", "bytes"})
        self.assertEqual((entry["role"], entry["scenario"], entry["wire"], entry["bytes"]), ("default", "ok", wire, len(raw)))


if __name__ == "__main__":
    unittest.main()
