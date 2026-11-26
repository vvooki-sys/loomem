"""The plan §8C review phase: frozen prompt, strict parser, pinned list, cap, blindness, and the whole
paid path against the loopback fake (dry-run and paid ``run``), 0 USD, temporary data only."""
import hashlib
import io
import itertools
import json
import os
from pathlib import Path
import tempfile
from types import SimpleNamespace
import unittest
from unittest import mock

import paid_dry_run
from paid_call_journal import CallJournal, request_id, sha256_hex
from paid_campaign import open_campaign
from paid_fake_openai import INPUT_TOKENS_PATH, PATH as CHAT_PATH
from paid_openai_wire import Transport, build_wire, wire_bytes
from paid_prompts import (REVIEW_PROMPT_SHA256, REVIEW_RESPONSE_SCHEMA, REVIEW_SYSTEM, REVIEW_USER_KEYS,
                          PromptsError, review_messages, review_response_format, review_stage)
from paid_review import (ReviewCapExceeded, ReviewListError, ReviewPlan, check_cap, judge_verdict_view,
                         parse_review, parse_review_list, plan_from_args)
from paid_runner import main as _main
from paid_test_support import PROFILES, write_holdout
from test_paid_preflight import SUBLIMITS, build_fixture, runner_env, tokenizer_available

SANDBOX_PRESENT = Path("/usr/bin/sandbox-exec").is_file()

ARM_NAMES = ("facts_retry", "selective_originals", "always_originals")
REFERENCE = {"reference_answer": "The workshop moved to Lisbon in March.",
             "required_answer_atoms": ["Lisbon"], "equivalence_rule": "city name", "unanswerable_explanation": None}
SOURCES = [{"conversation_date": "2025-07-10", "source_id": "h1-s1",
            "turns": [{"role": "user", "content": "We moved the workshop to Lisbon."}]}]


def main(argv, env=None, factories=None):
    out, err = io.StringIO(), io.StringIO()
    with mock.patch("sys.stderr", err):
        code = _main(argv, stdout=out, env=env, factories=factories, clock=itertools.count(1_700_000_000).__next__)
    return code, out.getvalue() + err.getvalue()


def review(**overrides):
    body = {"agrees_with_judge": True, "proposed_correction": "keep", "critical_error_types": [],
            "evidence": {"kind": "reference_atom", "source_id": None, "quote": "Lisbon"},
            "rationale": "The candidate names Lisbon, the reference city."}
    body.update(overrides)
    return json.dumps(body)


class PromptTests(unittest.TestCase):
    def test_fingerprint_is_the_stage_digest_and_the_plan_fingerprint_is_untouched(self):
        canonical = json.dumps({"system": REVIEW_SYSTEM, "response_schema": REVIEW_RESPONSE_SCHEMA,
                                "user_keys": list(REVIEW_USER_KEYS)}, sort_keys=True, ensure_ascii=False,
                               separators=(",", ":"), allow_nan=False)
        self.assertEqual(REVIEW_PROMPT_SHA256, hashlib.sha256(canonical.encode("utf-8")).hexdigest())
        self.assertEqual(review_stage().sha256, REVIEW_PROMPT_SHA256)
        from test_paid_preflight import build_fixture as fixture
        with tempfile.TemporaryDirectory() as tmp:
            prompts = fixture(tmp).prompts()
            self.assertEqual(set(prompts.fingerprint()),
                             {"diagnostic", "audit", "selection", "answer", "judge", "plan_json", "judge_prompt"})
            with self.assertRaises(PromptsError):
                prompts.stage("review")  # not a plan.json stage

    def test_frozen_texts_never_name_an_arm(self):
        frozen = json.dumps([REVIEW_SYSTEM, REVIEW_RESPONSE_SCHEMA, REVIEW_USER_KEYS])
        for word in ARM_NAMES + ("arm", "letter", "variant"):
            self.assertNotIn(word, frozen.lower().replace("alarm", ""), word)

    def test_messages_and_schema(self):
        parts = {key: f"v-{key}" for key in REVIEW_USER_KEYS}
        messages = review_messages(parts)
        self.assertEqual(messages[0], {"role": "system", "content": REVIEW_SYSTEM})
        self.assertEqual(list(json.loads(messages[1]["content"])), list(REVIEW_USER_KEYS))
        with self.assertRaises(PromptsError):
            review_messages({**parts, "arm": "facts_retry"})
        schema = review_response_format()
        self.assertEqual((schema["type"], schema["json_schema"]["strict"]), ("json_schema", True))
        self.assertFalse(REVIEW_RESPONSE_SCHEMA["additionalProperties"])
        self.assertEqual(set(REVIEW_RESPONSE_SCHEMA["required"]), set(REVIEW_RESPONSE_SCHEMA["properties"]))

    def test_judge_verdict_view_carries_no_bookkeeping(self):
        record = {"question_id": "q", "arm": "facts_retry", "candidate": "c", "request_id": "r", "usage": {},
                  "outcome": "ok", "score": "correct",
                  "verdict": {"valid": True, "status": "scored", "correct": True, "answer_span": "c",
                              "reason": "ok", "error": None}}
        self.assertEqual(judge_verdict_view(record), {"score": "correct", "status": "scored", "correct": True,
                                                      "answer_span": "c", "reason": "ok"})


class ParseReviewTests(unittest.TestCase):
    def parse(self, text, score="correct"):
        return parse_review(text, score, REFERENCE, SOURCES)

    def test_valid_agreement_and_disagreement(self):
        ok = self.parse(review())
        self.assertTrue(ok.valid, ok.error)
        self.assertEqual((ok.agrees_with_judge, ok.proposed_correction, ok.critical_error_types, ok.critical),
                         (True, "keep", [], False))
        flip = self.parse(review(agrees_with_judge=False, proposed_correction="incorrect",
                                 critical_error_types=["fabrication_after_abstention", "stale_value", "stale_value"],
                                 evidence={"kind": "source", "source_id": "h1-s1", "quote": "moved the workshop"}))
        self.assertTrue(flip.valid, flip.error)
        # every type, deduplicated, in plan §8A order; no priority
        self.assertEqual((flip.critical_error_types, flip.critical), (["stale_value", "fabrication_after_abstention"], True))
        short = self.parse(review(evidence={"kind": "reference_atom", "source_id": None, "quote": " Lis"}))
        self.assertTrue(short.valid, short.error)  # exactly 3 non-space characters
        invalid_judge = self.parse(review(agrees_with_judge=False, proposed_correction="correct"), "invalid_judge")
        self.assertTrue(invalid_judge.valid, invalid_judge.error)
        none = self.parse(review(evidence={"kind": "none", "source_id": None, "quote": ""}))
        self.assertTrue(none.valid, none.error)

    def test_invalid_reviews_are_recorded_not_raised(self):
        cases = {
            "not json": "{",
            "extra key": review(arm="facts_retry"),
            "duplicate key": '{"rationale": "a", ' + review()[1:],
            "agree with invalid judge": None,
            "disagree with keep": review(agrees_with_judge=False),
            "proposal equals judge": review(agrees_with_judge=False, proposed_correction="correct"),
            "agree with a correction": review(proposed_correction="incorrect"),
            "enum": review(critical_error_types=["minor"]),
            "not a list": review(critical_error_types="stale_value"),
            "old single field": review(critical_error_types=None),
            "quote too short": review(evidence={"kind": "reference_atom", "source_id": None, "quote": " L i"}),
            "source quote too short": review(evidence={"kind": "source", "source_id": "h1-s1", "quote": "We"}),
            "quote not in reference": review(evidence={"kind": "reference_atom", "source_id": None, "quote": "Porto"}),
            "reference with source id": review(evidence={"kind": "reference_atom", "source_id": "h1-s1", "quote": "Lisbon"}),
            "unknown source": review(evidence={"kind": "source", "source_id": "h9-s1", "quote": "Lisbon"}),
            "quote not in source": review(evidence={"kind": "source", "source_id": "h1-s1", "quote": "March"}),
            "none with quote": review(evidence={"kind": "none", "source_id": None, "quote": "x"}),
            "blank rationale": review(rationale=" "),
            "long rationale": review(rationale="x" * 1201),
        }
        for name, text in cases.items():
            score = "invalid_judge" if name == "agree with invalid judge" else "correct"
            with self.subTest(name):
                result = self.parse(text or review(), score)
                self.assertFalse(result.valid)
                self.assertTrue(result.error)


class ReviewListTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        from paid_holdout import verify_holdout
        root, manifest, always = write_holdout(self.tmp.name)
        self.holdout = verify_holdout(root, manifest, always)

    def raw(self, pairs, **extra):
        return json.dumps({"schema": 1, "pairs": pairs, **extra}).encode("utf-8")

    def test_parse_and_refusals(self):
        pairs = [{"question_id": "h1-q01", "arm": "always_originals"}, {"question_id": "h1-q02", "arm": "facts_retry"}]
        self.assertEqual(parse_review_list(self.raw(pairs, seed="s"), self.holdout),
                         (("h1-q01", "always_originals"), ("h1-q02", "facts_retry")))
        bad = {"empty": self.raw([]), "twice": self.raw(pairs + pairs[:1]),
               "unknown arm": self.raw([{"question_id": "h1-q01", "arm": "F"}]),
               "unknown question": self.raw([{"question_id": "h9-q01", "arm": "facts_retry"}]),
               "always outside the list": self.raw([{"question_id": "h1-q02", "arm": "always_originals"}]),
               "extra pair key": self.raw([{"question_id": "h1-q01", "arm": "facts_retry", "path": "x"}]),
               "schema": json.dumps({"schema": 2, "pairs": pairs}).encode(), "not json": b"[",
               "duplicate key": b'{"schema": 1, "schema": 1, "pairs": []}'}
        for name, raw in bad.items():
            with self.subTest(name), self.assertRaises(ReviewListError):
                parse_review_list(raw, self.holdout)

    def test_plan_from_args_needs_phase_review_and_every_pin(self):
        path = Path(self.tmp.name) / "list.json"
        path.write_bytes(self.raw([{"question_id": "h1-q01", "arm": "facts_retry"}]))
        sha = hashlib.sha256(path.read_bytes()).hexdigest()
        args = lambda phase, *pins: SimpleNamespace(phase=phase, review_list=pins[0], review_list_sha256=pins[1],
                                                    review_prompt_sha256=pins[2])
        self.assertIsNone(plan_from_args(args("all", None, None, None), self.holdout))
        plan = plan_from_args(args("review", str(path), sha, REVIEW_PROMPT_SHA256), self.holdout)
        self.assertEqual((plan.sha256, plan.pairs, len(plan)), (sha, (("h1-q01", "facts_retry"),), 1))
        refusals = [args("all", str(path), sha, REVIEW_PROMPT_SHA256), args("review", str(path), sha, None),
                    args("review", str(path), "0" * 64, REVIEW_PROMPT_SHA256),
                    args("review", str(path), sha, "0" * 64), args("review", str(path) + ".x", sha, REVIEW_PROMPT_SHA256)]
        for refused in refusals:
            with self.subTest(refused=refused), self.assertRaises(ReviewListError):
                plan_from_args(refused, self.holdout)


class CapTests(unittest.TestCase):
    def state(self, *pairs):
        return SimpleNamespace(intents={f"r{i}": {"stage": "review", "binding": {"question_id": q, "arm": a}}
                                        for i, (q, a) in enumerate(pairs)})

    def test_cap_is_the_frozen_list(self):
        plan = ReviewPlan("0" * 64, (("q1", "facts_retry"), ("q2", "facts_retry")), b"", REVIEW_PROMPT_SHA256)
        check_cap(self.state(), plan, "q1", "facts_retry")
        check_cap(self.state(("q1", "facts_retry")), plan, "q1", "facts_retry")  # a replay is no new review
        check_cap(self.state(("q1", "facts_retry")), plan, "q2", "facts_retry")
        with self.assertRaises(ReviewCapExceeded):  # a journal review outside the list
            check_cap(self.state(("q3", "facts_retry")), plan, "q1", "facts_retry")
        short = ReviewPlan("0" * 64, (("q1", "facts_retry"),), b"", REVIEW_PROMPT_SHA256)
        with self.assertRaises(ReviewCapExceeded):  # one more than the list would allow
            check_cap(self.state(("q1", "facts_retry")), short, "q2", "facts_retry")


    def test_review_and_judge_requests_are_not_arm_spend(self):
        from paid_phases import _arm_reserved_before
        bind = lambda stage: {"binding": {"question_id": "q1", "arm": "facts_retry", "stage": stage}}
        state = SimpleNamespace(intents={"r1": bind("review"), "r2": bind("judge")}, settled={})
        ctx = SimpleNamespace(journal=SimpleNamespace(state=state))
        self.assertFalse(_arm_reserved_before(ctx, "q1", "facts_retry"))
        state.intents["r3"] = bind("diagnostic")
        self.assertTrue(_arm_reserved_before(ctx, "q1", "facts_retry"))


class _FakeCapture:
    """Wraps paid_dry_run.fake_provider: keeps every fake instance; optional count scenarios."""

    def __init__(self, count_scenarios=None):
        self.fakes, self.count_scenarios = [], count_scenarios
        self.original = paid_dry_run.fake_provider

    def __call__(self, prompts, profiles, scenarios):
        fake = self.original(prompts, profiles, scenarios)
        if self.count_scenarios:
            fake.count_scenarios = dict(self.count_scenarios)
        self.fakes.append(fake)
        return fake


class ReviewDryRunTests(unittest.TestCase):
    def setUp(self):
        if not tokenizer_available():
            self.skipTest("o200k cache unavailable (TIKTOKEN_CACHE_DIR)")
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.fixture = build_fixture(self.root)
        self.campaign_dir = self.root / "dry"
        self.env = runner_env()
        patch = mock.patch("paid_preflight.MIN_FREE_BYTES", 1)
        patch.start()
        self.addCleanup(patch.stop)
        self.capture = _FakeCapture()
        patch = mock.patch("paid_dry_run.fake_provider", self.capture)
        patch.start()
        self.addCleanup(patch.stop)

    def dry_run(self, *extra, sublimits=None):
        return main(["dry-run", *self.fixture.flags(self.campaign_dir, sublimits=sublimits), "--fake-engine", *extra],
                    env=self.env)

    def write_list(self, pairs, name="review_list.json"):
        path = self.root / name
        path.write_bytes(json.dumps({"schema": 1, "seed": "test",
                                     "pairs": [{"question_id": q, "arm": a} for q, a in pairs]}).encode("utf-8"))
        return path, hashlib.sha256(path.read_bytes()).hexdigest()

    def review_flags(self, path, sha, prompt=REVIEW_PROMPT_SHA256):
        return ["--resume", "--phase", "review", "--review-list", str(path), "--review-list-sha256", sha,
                "--review-prompt-sha256", prompt]

    def roles(self, fake, path=CHAT_PATH):
        return [e["role"] for e in fake.raw_log if e["path"] == path]

    def review_intents(self):
        return {r: i for r, i in CallJournal(self.campaign_dir).state.intents.items() if i["stage"] == "review"}

    def test_phase_all_never_reviews_then_review_is_blind_reserved_and_resumable(self):
        code, out = self.dry_run()
        self.assertEqual(code, 0, out)
        summary = json.loads((self.campaign_dir / "dry_run_summary.json").read_text())
        self.assertNotIn("review", summary)
        self.assertEqual(len(summary["preflight"]), 17)
        self.assertNotIn("review", self.roles(self.capture.fakes[0]))
        self.assertFalse((self.campaign_dir / "reviews").exists() or (self.campaign_dir / "review").exists())
        self.assertNotIn("review", json.loads((self.campaign_dir / "reports" / "report-1.json").read_text()))

        pairs = [("h1-q01", "facts_retry"), ("h1-q01", "always_originals"), ("h1-q02", "selective_originals")]
        path, sha = self.write_list(pairs)
        seen = []
        real = Transport.__call__

        def spy(transport, body, timeout_s):  # what the journal and ledger hold at the moment of HTTP
            wire_sha = hashlib.sha256(body).hexdigest()
            journal = CallJournal(self.campaign_dir)
            # Two arms with the same answer send identical bytes (blind) under two request ids (arm in the binding).
            [rid] = [r for r, i in journal.state.intents.items()
                     if i["wire_sha256"] == wire_sha and r not in journal.state.settled]
            ledger = json.loads((self.campaign_dir / "budget.json").read_text())["requests"]
            seen.append((journal.state.intents[rid]["stage"], rid in journal.state.sent,
                         rid in ledger and ledger[rid]["actual_usd"] is None))
            return real(transport, body, timeout_s)

        with mock.patch.object(Transport, "__call__", spy):
            code, out = self.dry_run(*self.review_flags(path, sha))
        self.assertEqual(code, 0, out)
        self.assertIn("PASS  18 review prompt + list pins", out)
        self.assertEqual(seen, [("review", True, True)] * 3)  # reserved + sent before every HTTP
        fake = self.capture.fakes[1]
        self.assertEqual(self.roles(fake), ["review"] * 3)
        self.assertEqual(self.roles(fake, INPUT_TOKENS_PATH), ["review"] * 3)  # exact count before each
        forbidden = ARM_NAMES + ('"arm"', '"letter"', '"F"', '"S"', '"A"', str(self.root), "verdicts/", "arms/",
                                 "reviews/", ".json")
        for entry in fake.raw_log:
            text = entry["raw"].decode("utf-8")
            for word in forbidden:
                self.assertNotIn(word, text, f"{entry['path']}: model-visible bytes contain {word!r}")
        chat = [e["raw"] for e in fake.raw_log if e["path"] == CHAT_PATH]
        self.assertEqual(chat[0], chat[1], "same answer under two arms: byte-identical model-visible input")
        user = json.loads(json.loads(chat[0])["messages"][1]["content"])
        self.assertEqual(list(user), list(REVIEW_USER_KEYS))
        self.assertIn("GOLD-REFERENCE", json.dumps(user["reference"]))
        self.assertEqual(user["judge_verdict"]["score"], "correct")
        intents = self.review_intents()
        self.assertEqual(len(intents), 3)
        self.assertEqual({i["letter"] for i in intents.values()}, {"R"})
        self.assertEqual({(i["binding"]["question_id"], i["binding"]["arm"]) for i in intents.values()}, set(pairs))
        settled = CallJournal(self.campaign_dir).state.settled
        self.assertEqual({settled[r]["model_observed"] for r in intents}, {"gpt-6.2-astra"})
        self.assertTrue(all(i["o_tokens"] == PROFILES["review"]["max_completion_tokens"] for i in intents.values()))
        for qid, arm in pairs:
            record = json.loads((self.campaign_dir / "reviews" / f"{qid}.{arm}.json").read_text())
            self.assertEqual((record["status"], record["model_observed"], record["review_list_sha256"]),
                             ("reviewed", "gpt-6.2-astra", sha))
            self.assertTrue(record["review"]["valid"], record)
            verdict = json.loads((self.campaign_dir / "verdicts" / f"{qid}.{arm}.json").read_text())
            self.assertEqual(record["judge"], {"request_id": verdict["request_id"], "score": verdict["score"]})
        frozen = json.loads((self.campaign_dir / "review" / "frozen.json").read_text())
        self.assertEqual((frozen["review_list_sha256"], frozen["pairs"]), (sha, 3))
        self.assertEqual((self.campaign_dir / "review" / "list.json").read_bytes(), path.read_bytes())
        summary = json.loads((self.campaign_dir / "dry_run_summary.json").read_text())
        self.assertEqual(summary["review"]["artifacts"], 3)
        self.assertEqual(summary["review"]["summary"]["by_status"], {"reviewed": 3})
        verdicts_before = {p.name: p.read_bytes() for p in (self.campaign_dir / "verdicts").glob("*.json")}

        code, out = self.dry_run(*self.review_flags(path, sha))  # resume: nothing new leaves
        self.assertEqual(code, 0, out)
        self.assertIn("frozen", out)
        self.assertEqual(self.capture.fakes[2].raw_log, [])
        self.assertEqual(len(self.review_intents()), 3)
        self.assertEqual({p.name: p.read_bytes() for p in (self.campaign_dir / "verdicts").glob("*.json")},
                         verdicts_before)  # a review never changes a verdict

        other, other_sha = self.write_list([("h1-q02", "facts_retry")], "other.json")
        code, out = self.dry_run(*self.review_flags(other, other_sha))
        self.assertEqual(code, 2, out)
        self.assertIn("one list per campaign", out)
        code, out = self.dry_run(*self.review_flags(path, "0" * 64))
        self.assertEqual(code, 2, out)
        self.assertIn("SHA-256 mismatch", out)
        code, out = self.dry_run(*self.review_flags(path, sha, prompt="1" * 64))
        self.assertEqual(code, 2, out)
        self.assertIn("review prompt", out)
        self.assertTrue(all(f.raw_log == [] for f in self.capture.fakes[3:]))  # refused before any request
        self.assertEqual(len(self.review_intents()), 3)

    def test_review_flags_are_refused_outside_phase_review_and_review_needs_resume(self):
        path, sha = self.write_list([("h1-q01", "facts_retry")])
        code, out = self.dry_run("--review-list", str(path), "--review-list-sha256", sha)
        self.assertEqual(code, 2, out)
        self.assertIn("need --phase review", out)
        code, out = self.dry_run("--phase", "review", "--review-list", str(path), "--review-list-sha256", sha,
                                 "--review-prompt-sha256", REVIEW_PROMPT_SHA256)
        self.assertEqual(code, 2, out)
        self.assertIn("needs --resume", out)
        self.assertFalse(self.campaign_dir.exists())

    def test_model_change_stops(self):
        self.assertEqual(self.dry_run()[0], 0)
        path, sha = self.write_list([("h1-q01", "facts_retry"), ("h1-q02", "facts_retry")])
        code, out = self.dry_run(*self.review_flags(path, sha), "--scenario", "review=model_drift")
        self.assertEqual(code, 3, out)
        self.assertIn("ModelDrift", out)
        [(rid, _)] = self.review_intents().items()
        self.assertEqual(CallJournal(self.campaign_dir).state.settled[rid]["outcome"], "model_drift")
        self.assertFalse((self.campaign_dir / "reviews").exists())

    def test_input_above_the_standard_tier_is_refused_before_any_reservation(self):
        self.assertEqual(self.dry_run()[0], 0)
        path, sha = self.write_list([("h1-q01", "facts_retry")])
        self.capture.count_scenarios = {"review": "count_over_bound"}
        code, out = self.dry_run(*self.review_flags(path, sha))
        self.assertEqual(code, 3, out)
        self.assertIn("272000-token standard-tier hard limit", out)
        self.assertEqual(self.roles(self.capture.fakes[1]), [])  # counted, never sent
        self.assertEqual(self.roles(self.capture.fakes[1], INPUT_TOKENS_PATH), ["review"])
        self.assertEqual(self.review_intents(), {})

    def test_sublimit_r_must_cover_the_whole_list(self):
        self.assertEqual(self.dry_run(sublimits={**SUBLIMITS, "R": "0"})[0], 0)
        path, sha = self.write_list([("h1-q01", "facts_retry")])
        code, out = self.dry_run(*self.review_flags(path, sha), sublimits={**SUBLIMITS, "R": "0"})
        self.assertEqual(code, 2, out)
        self.assertIn("exceeds sublimit R 0", out)
        self.assertEqual(self.capture.fakes[1].raw_log, [])

    def test_a_review_outside_the_frozen_list_stops_before_any_new_request(self):
        self.assertEqual(self.dry_run()[0], 0)
        self.forge_review_intent("h1-q02", "facts_retry")
        path, sha = self.write_list([("h1-q01", "facts_retry")])
        code, out = self.dry_run(*self.review_flags(path, sha))
        self.assertEqual(code, 3, out)
        self.assertIn("ReviewCapExceeded", out)
        self.assertEqual(self.capture.fakes[1].raw_log, [])
        self.assertEqual(len(self.review_intents()), 1)

    def test_a_review_outside_the_frozen_list_stops_a_resume_with_nothing_left_to_review(self):
        self.assertEqual(self.dry_run()[0], 0)
        path, sha = self.write_list([("h1-q01", "facts_retry")])
        self.assertEqual(self.dry_run(*self.review_flags(path, sha))[0], 0)  # every listed pair has its artifact
        self.forge_review_intent("h1-q02", "facts_retry")
        before = len(self.capture.fakes)
        code, out = self.dry_run(*self.review_flags(path, sha))
        self.assertEqual(code, 3, out)
        self.assertIn("ReviewCapExceeded", out)
        self.assertIn("outside the frozen list", out)
        self.assertTrue(all(f.raw_log == [] for f in self.capture.fakes[before:]))

    def test_unjudged_pairs_are_refused_by_preflight(self):
        self.assertEqual(self.dry_run()[0], 0)
        (self.campaign_dir / "verdicts" / "h1-q02.facts_retry.json").unlink()
        path, sha = self.write_list([("h1-q01", "facts_retry"), ("h1-q02", "facts_retry")])
        code, out = self.dry_run(*self.review_flags(path, sha))
        self.assertEqual(code, 2, out)
        self.assertIn("FAIL  18 review prompt + list pins", out)
        self.assertIn("no verdict", out)

    def test_an_interrupted_freeze_is_recovered_only_for_the_same_bytes(self):
        self.assertEqual(self.dry_run()[0], 0)
        path, sha = self.write_list([("h1-q01", "facts_retry")])
        copy = self.campaign_dir / "review" / "list.json"
        copy.parent.mkdir()
        copy.write_bytes(path.read_bytes() + b" ")  # crash after list.json, different bytes: refused
        code, out = self.dry_run(*self.review_flags(path, sha))
        self.assertEqual(code, 2, out)
        self.assertIn("interrupted freeze differs", out)
        copy.write_bytes(path.read_bytes())  # crash between list.json and frozen.json: recovered
        code, out = self.dry_run(*self.review_flags(path, sha))
        self.assertEqual(code, 0, out)
        frozen = json.loads((self.campaign_dir / "review" / "frozen.json").read_text())
        self.assertEqual(frozen["review_list_sha256"], sha)
        self.assertTrue((self.campaign_dir / "reviews" / "h1-q01.facts_retry.json").is_file())

    def test_length_and_invalid_replies_are_final_artifacts_with_the_raw_text(self):
        self.assertEqual(self.dry_run()[0], 0)
        path, sha = self.write_list([("h1-q01", "facts_retry"), ("h1-q02", "facts_retry")])
        bad = json.dumps({"agrees_with_judge": True, "proposed_correction": "incorrect", "critical_error_types": [],
                          "evidence": {"kind": "none", "source_id": None, "quote": ""}, "rationale": "r"})
        with mock.patch("paid_fake_openai._review_for", side_effect=lambda user: json.loads(bad)):
            code, out = self.dry_run(*self.review_flags(path, sha), "--scenario", "review=length,ok")
        self.assertEqual(code, 0, out)
        length = json.loads((self.campaign_dir / "reviews" / "h1-q01.facts_retry.json").read_text())
        self.assertEqual((length["status"], length["outcome"], length["completion_text"]), ("length", "length", ""))
        self.assertFalse(length["review"]["valid"])
        invalid = json.loads((self.campaign_dir / "reviews" / "h1-q02.facts_retry.json").read_text())
        self.assertEqual(invalid["status"], "invalid_review")
        self.assertEqual(json.loads(invalid["completion_text"]), json.loads(bad))
        self.assertIn("agreement needs", invalid["review"]["error"])
        code, out = self.dry_run(*self.review_flags(path, sha))  # final: no re-review on resume
        self.assertEqual((code, self.capture.fakes[-1].raw_log), (0, []))
        report = sorted((self.campaign_dir / "reports").glob("report-*.json"))[-1]
        self.assertEqual(json.loads(report.read_text())["review"]["by_status"], {"invalid_review": 1, "length": 1})

    def test_rejected_review_is_sent_again_only_with_retry_rejected(self):
        self.assertEqual(self.dry_run()[0], 0)
        path, sha = self.write_list([("h1-q01", "facts_retry")])
        code, out = self.dry_run(*self.review_flags(path, sha), "--scenario", "review=rejected_4xx")
        self.assertEqual(code, 3, out)
        self.assertIn("Rejected4xx", out)
        [(rid, intent)] = self.review_intents().items()
        code, out = self.dry_run(*self.review_flags(path, sha))  # without the flag: replays as a stop
        self.assertEqual(code, 3, out)
        self.assertIn(f"pass --retry-rejected {rid}", out)
        self.assertEqual(self.roles(self.capture.fakes[-1]), [])
        code, out = self.dry_run(*self.review_flags(path, sha), "--retry-rejected", rid)
        self.assertEqual(code, 0, out)
        self.assertEqual(self.roles(self.capture.fakes[-1]), ["review"])
        intents = self.review_intents()
        [retried] = [r for r in intents if r != rid]
        self.assertEqual(intents[retried]["binding"], {**intent["binding"], "attempt": 2})
        record = json.loads((self.campaign_dir / "reviews" / "h1-q01.facts_retry.json").read_text())
        self.assertEqual((record["status"], record["request_id"]), ("reviewed", retried))

    def forge_review_intent(self, qid, arm):
        """A review request for a pair, settled 0 USD (rejected), as an earlier rogue call would leave it."""
        sheet = self.fixture.sheet()
        campaign = open_campaign(self.campaign_dir, sheet)
        journal = CallJournal(self.campaign_dir)
        wire = build_wire(campaign.profile("review"), review_messages({k: "x" for k in REVIEW_USER_KEYS}))
        bind = {"history_id": "h1", "session_id": None, "question_id": qid, "arm": arm, "stage": "review",
                "attempt": 1, "manifest_sha256": campaign.spec["holdout"]["manifest_sha256"],
                "context_sha256": "0" * 64, "prompt_sha256": REVIEW_PROMPT_SHA256}
        body = wire_bytes(wire)
        rid = request_id(campaign.campaign_id, bind, sha256_hex(body))
        bound = sheet.stage_profile("review", 1000, True)
        max_usd = str(sheet.max_usd(bound, "p1"))
        token = campaign.ledger.reserve(rid, max_usd)
        journal.intent({"schema": 1, "request_id": rid, "letter": "R", "stage": "review", "binding": bind,
                        "max_usd": max_usd, "wire_sha256": sha256_hex(body), "ledger_token": token,
                        "profile": campaign.profile("review"), "r_tokens": bound.r_tokens,
                        "o_tokens": bound.o_tokens}, wire)
        journal.sent(rid, 1, 5, "127.0.0.1")
        journal.settled(rid, {"http_status": 429, "model_observed": None, "usage": None, "finish_reason": None,
                              "outcome": "rejected_4xx", "actual_usd": "0"},
                        {"status": 429, "x_request_id": None, "body_text": "{}"})
        campaign.ledger.settle(token, "0")


@unittest.skipUnless(SANDBOX_PRESENT, "sandbox-exec absent: paid-mode preflight cannot pass here")
class PaidRunReviewTests(unittest.TestCase):
    """``run --phase review`` on a paid campaign whose provider is the loopback fake."""

    def setUp(self):
        if not tokenizer_available():
            self.skipTest("o200k cache unavailable (TIKTOKEN_CACHE_DIR)")
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.fixture = build_fixture(self.root)
        self.campaign_dir = self.root / "paid"
        for target, value in (("paid_preflight.MIN_FREE_BYTES", 1),):
            patch = mock.patch(target, value)
            patch.start()
            self.addCleanup(patch.stop)
        self.fake = paid_dry_run.fake_provider(self.fixture.prompts(), PROFILES, {})
        self.fake.__enter__()
        self.addCleanup(self.fake.__exit__, None, None, None)
        probe = mock.patch("paid_preflight_engine.probe_engine",
                           return_value={"passed": True, "problems": [], "observed": {
                               "system_bytes": 1, "wire_overhead_bytes": 1, "sheet_system_extraction": 6000,
                               "system_sha256": "0" * 64, "readiness_bad_body_status": 422}})
        probe.start()
        self.addCleanup(probe.stop)
        self.env = runner_env(OPENAI_API_KEY=self.fake.token)
        env_patch = mock.patch.dict(os.environ, {"OPENAI_API_KEY": self.fake.token})
        env_patch.start()
        self.addCleanup(env_patch.stop)
        code, out = main(["create-campaign", *self.fixture.flags(self.campaign_dir, extra=[
            "--provider-base-url", self.fake.url, "--api-key-env", "OPENAI_API_KEY"])], env=self.env)
        self.assertEqual(code, 0, out)

    def run_phase(self, *extra):
        from paid_fake_engine import fake_engine_factory
        return main(["run", "--campaign-dir", str(self.campaign_dir), *extra], env=self.env,
                    factories={"engine": fake_engine_factory})

    def test_all_then_review_then_resume(self):
        code, out = self.run_phase("--phase", "all")
        self.assertEqual(code, 0, out)
        self.assertNotIn("review", [e["role"] for e in self.fake.log])
        path = self.root / "list.json"
        path.write_bytes(json.dumps({"schema": 1, "pairs": [{"question_id": "h1-q02", "arm": "selective_originals"}]}).encode())
        sha = hashlib.sha256(path.read_bytes()).hexdigest()
        flags = ["--phase", "review", "--review-list", str(path), "--review-list-sha256", sha,
                 "--review-prompt-sha256", REVIEW_PROMPT_SHA256]
        code, out = self.run_phase("--phase", "judge", *flags[2:])
        self.assertEqual(code, 2, out)
        self.assertIn("need --phase review", out)
        before = len(self.fake.log)
        code, out = self.run_phase(*flags)
        self.assertEqual(code, 0, out)
        self.assertIn("PASS  18 review prompt + list pins", out)
        self.assertEqual([e["role"] for e in self.fake.log[before:]], ["review"])
        record = json.loads((self.campaign_dir / "reviews" / "h1-q02.selective_originals.json").read_text())
        self.assertEqual(record["status"], "reviewed")
        report = sorted((self.campaign_dir / "reports").glob("report-*.json"))[-1]
        self.assertEqual(json.loads(report.read_text())["review"]["by_status"], {"reviewed": 1})
        code, out = self.run_phase(*flags)
        self.assertEqual(code, 0, out)
        self.assertEqual(len(self.fake.log), before + 1)
        self.assertIn("PASS  11 gold leak scan", out)  # review requests carry gold by design and are not scanned

    def test_acknowledged_uncertain_review_is_a_technical_error_artifact(self):
        self.assertEqual(self.run_phase("--phase", "all")[0], 0)
        path = self.root / "list.json"
        path.write_bytes(json.dumps({"schema": 1, "pairs": [{"question_id": "h1-q01", "arm": "facts_retry"},
                                                            {"question_id": "h1-q02", "arm": "facts_retry"}]}).encode())
        sha = hashlib.sha256(path.read_bytes()).hexdigest()
        flags = ["--phase", "review", "--review-list", str(path), "--review-list-sha256", sha,
                 "--review-prompt-sha256", REVIEW_PROMPT_SHA256]
        self.fake.scenarios["review"] = "timeout"  # connection closed: outcome unknown
        code, out = self.run_phase(*flags)
        self.assertEqual(code, 3, out)
        state = CallJournal(self.campaign_dir).state
        [rid] = list(state.uncertain)
        self.assertEqual(state.intents[rid]["stage"], "review")
        self.fake.scenarios["review"] = "ok"
        code, out = self.run_phase(*flags)
        self.assertEqual(code, 2, out)  # blocked until acknowledged
        self.assertIn("not acknowledged", out)
        sent = [e["role"] for e in self.fake.log].count("review")
        code, out = self.run_phase(*flags, "--acknowledge-uncertain", rid)
        self.assertEqual(code, 0, out)
        acked = json.loads((self.campaign_dir / "reviews" / "h1-q01.facts_retry.json").read_text())
        self.assertEqual((acked["status"], acked["request_id"], acked["outcome"]),
                         ("technical_error", rid, f"acknowledged_uncertain:{rid}"))
        other = json.loads((self.campaign_dir / "reviews" / "h1-q02.facts_retry.json").read_text())
        self.assertEqual(other["status"], "reviewed")
        self.assertEqual([e["role"] for e in self.fake.log].count("review"), sent + 1)  # the acknowledged one never again


if __name__ == "__main__":
    unittest.main()
