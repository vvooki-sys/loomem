"""paid_judge: reference payload, source conversations, strict 4-field parser, one call per arm."""
import json
from pathlib import Path
import re
import unittest

from paid_arms import ArmOutcome, run_arm
from paid_call_journal import JournalError
from paid_context import ContextError
from paid_holdout import leaks_gold
from paid_judge import (REFERENCE_FIELDS, SOURCE_SCOPE, Verdict, judge_arm, judge_messages, parse_verdict,
                        read_verdict, reference_payload, score, source_conversations, write_verdict)
from test_paid_arms import ArmHarness

HERE = Path(__file__).resolve().parent
GOLD = {"reference_answer": "Paris", "required_answer_atoms": ["Paris"], "equivalence_rule": "city name",
        "unanswerable_explanation": None, "category": "single-session-user"}


def verdict_text(status="scored", correct=True, span="Synthetic", reason="fits", **extra):
    return json.dumps({"status": status, "correct": correct, "answer_span": span, "reason": reason, **extra})


class ParserTests(unittest.TestCase):
    CANDIDATE = "Synthetic answer."

    def test_valid_scored_verdicts(self):
        verdict = parse_verdict(verdict_text(), self.CANDIDATE)
        self.assertEqual((verdict.valid, verdict.error, score(verdict)), (True, None, "correct"))
        wrong = parse_verdict(verdict_text(correct=False, span="answer."), self.CANDIDATE)
        self.assertEqual((wrong.valid, score(wrong)), (True, "incorrect"))
        self.assertEqual(set(wrong.to_dict()), {"valid", "status", "correct", "answer_span", "reason", "error"})

    def test_status_outside_scored_is_invalid_judge(self):
        other = parse_verdict(verdict_text(status="unscorable", correct=None, span=""), self.CANDIDATE)
        self.assertEqual((other.valid, score(other)), (True, "invalid_judge"))
        enum = parse_verdict(verdict_text(status="unscorable", correct=None, span=""), self.CANDIDATE, statuses=("scored",))
        self.assertEqual((enum.valid, score(enum)), (False, "invalid_judge"))
        self.assertFalse(parse_verdict(verdict_text(status="unscorable", span=""), self.CANDIDATE).valid)  # correct not null
        self.assertFalse(parse_verdict(verdict_text(correct=None), self.CANDIDATE).valid)  # scored needs a bool
        self.assertFalse(parse_verdict(verdict_text(correct="yes"), self.CANDIDATE).valid)

    def test_span_must_be_exact_nonblank_substring(self):
        self.assertFalse(parse_verdict(verdict_text(span="synthetic"), self.CANDIDATE).valid)  # case differs
        self.assertFalse(parse_verdict(verdict_text(span="   "), self.CANDIDATE).valid)
        self.assertFalse(parse_verdict(verdict_text(span=""), self.CANDIDATE).valid)
        self.assertFalse(parse_verdict(verdict_text(span=42), self.CANDIDATE).valid)
        self.assertTrue(parse_verdict(verdict_text(span=self.CANDIDATE), self.CANDIDATE).valid)

    def test_reason_limits(self):
        self.assertTrue(parse_verdict(verdict_text(reason="x" * 600), self.CANDIDATE).valid)
        self.assertFalse(parse_verdict(verdict_text(reason="x" * 601), self.CANDIDATE).valid)
        self.assertFalse(parse_verdict(verdict_text(reason="  "), self.CANDIDATE).valid)
        self.assertFalse(parse_verdict(verdict_text(reason=None), self.CANDIDATE).valid)

    def test_blank_candidate(self):
        self.assertFalse(parse_verdict(verdict_text(correct=True, span=""), "").valid)
        self.assertFalse(parse_verdict(verdict_text(correct=False, span="x"), "   ").valid)
        blank = parse_verdict(verdict_text(correct=False, span=""), "")
        self.assertEqual((blank.valid, score(blank)), (True, "incorrect"))

    def test_shape_strictness(self):
        for text in ("not json", "[]", '{"status":"scored"}', verdict_text(extra="x"),
                     '{"status":"scored","correct":true,"answer_span":"Synthetic","reason":"r","status":"scored"}'):
            with self.subTest(text=text):
                verdict = parse_verdict(text, self.CANDIDATE)
                self.assertEqual((verdict.valid, score(verdict)), (False, "invalid_judge"))
        self.assertFalse(parse_verdict(verdict_text(), None).valid)
        self.assertEqual(score(Verdict(False, None, None, None, None, "x")), "invalid_judge")


class PayloadTests(unittest.TestCase):
    def test_reference_payload_is_the_four_gold_fields(self):
        reference = reference_payload(GOLD)
        self.assertEqual(tuple(reference), REFERENCE_FIELDS)
        self.assertNotIn("category", reference)
        with self.assertRaisesRegex(ContextError, "above"):
            reference_payload(GOLD, cap_bytes=10)
        with self.assertRaises(ContextError):
            reference_payload({"reference_answer": "x"})

    def test_source_conversations_shape(self):
        history = {"history_id": "h", "sessions": [
            {"session_id": "s1", "conversation_date": "2025-01-01", "turns": [{"role": "user", "content": "a", "extra": 1}]},
            {"session_id": "s2", "conversation_date": "2025-01-02", "turns": [{"role": "assistant", "content": "b"}]}]}
        self.assertEqual(source_conversations(history), [
            {"conversation_date": "2025-01-01", "source_id": "s1", "turns": [{"role": "user", "content": "a"}]},
            {"conversation_date": "2025-01-02", "source_id": "s2", "turns": [{"role": "assistant", "content": "b"}]}])
        with self.assertRaises(ContextError):
            source_conversations({"sessions": []})

    def test_judge_messages_follow_the_pinned_key_order(self):
        h = ArmHarness(self)
        messages = judge_messages(h.prompts, "Q?", reference_payload(GOLD), "cand", [], SOURCE_SCOPE)
        self.assertEqual(list(json.loads(messages[1]["content"])),
                         ["source_scope", "source_conversations", "question", "reference", "candidate"])
        self.assertEqual(messages[0]["content"], h.prompts.stage("judge").system)

    def test_only_paid_judge_references_judge_view(self):
        offenders = []
        for path in sorted(HERE.glob("*.py")):
            if path.name.startswith("test_") or path.name in ("paid_judge.py", "paid_holdout.py"):
                continue
            if re.search(r"\bjudge_view\b", path.read_text(encoding="utf-8")):
                offenders.append(path.name)
        self.assertEqual(offenders, [])
        self.assertIn("judge_view", (HERE / "paid_judge.py").read_text(encoding="utf-8"))


class JudgeArmTests(unittest.TestCase):
    def test_judge_call_carries_reference_and_scores(self):
        h = ArmHarness(self)
        ctx = h.context("h1-q01")
        outcome = run_arm("facts_retry", ctx, h.deps)
        record = judge_arm(h.deps, h.holdout, ctx, "facts_retry", outcome)
        self.assertEqual((record["score"], record["outcome"], record["candidate"]), ("correct", "ok", "Synthetic answer."))
        self.assertEqual(set(record), {"question_id", "arm", "candidate", "verdict", "score", "request_id", "usage", "outcome"})
        self.assertTrue(record["verdict"]["valid"])
        intent = h.journal.state.intents[record["request_id"]]
        self.assertEqual((intent["letter"], intent["stage"], intent["binding"]["arm"]), ("F", "judge", "facts_retry"))
        self.assertEqual((intent["binding"]["context_sha256"], intent["binding"]["prompt_sha256"]),
                         (ctx["context_sha256"], h.prompts.stage("judge").sha256))
        request = json.loads(h.journal.path(record["request_id"], "request").read_text())
        self.assertEqual(request["model"], "gpt-6.2-astra")
        user = json.loads(request["messages"][1]["content"])
        self.assertEqual(list(user), ["source_scope", "source_conversations", "question", "reference", "candidate"])
        self.assertEqual(user["source_scope"], SOURCE_SCOPE)
        self.assertEqual(len(user["source_conversations"]), 2)
        self.assertTrue(leaks_gold(json.dumps(request), h.holdout.gold_markers("h1-q01")))
        self.assertEqual(h.roles(), ["diagnostic", "answer", "judge"])

    def test_two_arms_same_candidate_two_judge_requests(self):
        h = ArmHarness(self)
        ctx = h.context("h1-q01")
        records = [judge_arm(h.deps, h.holdout, ctx, arm, run_arm(arm, ctx, h.deps)) for arm in ("facts_retry", "always_originals")]
        self.assertEqual(records[0]["candidate"], records[1]["candidate"])
        self.assertNotEqual(records[0]["request_id"], records[1]["request_id"])
        self.assertEqual(h.roles().count("judge"), 2)
        self.assertEqual({h.journal.state.intents[r["request_id"]]["letter"] for r in records}, {"F", "A"})

    def test_failed_arms_are_not_judged(self):
        h = ArmHarness(self, scenarios={"answer": "length"})
        ctx = h.context("h1-q01")
        outcome = run_arm("facts_retry", ctx, h.deps)
        record = judge_arm(h.deps, h.holdout, ctx, "facts_retry", outcome)
        self.assertEqual((record["score"], record["request_id"], record["verdict"]), ("length", None, None))
        broken = ArmOutcome("h1-q01", "facts_retry", "technical_error", error="boom")
        record = judge_arm(h.deps, h.holdout, ctx, "facts_retry", broken)
        self.assertEqual((record["score"], record["outcome"]), ("technical_error", "technical_error"))
        self.assertNotIn("judge", h.roles())
        with self.assertRaises(ContextError):
            judge_arm(h.deps, h.holdout, ctx, "selective_originals", broken)

    def test_truncated_or_invalid_judge_output_is_invalid_judge(self):
        h = ArmHarness(self, scenarios={"judge": "length"})
        ctx = h.context("h1-q01")
        record = judge_arm(h.deps, h.holdout, ctx, "facts_retry", run_arm("facts_retry", ctx, h.deps))
        self.assertEqual(record["score"], "invalid_judge")
        self.assertIn("length", record["verdict"]["error"])
        self.assertIsNotNone(record["request_id"])

    def test_verdict_records_are_written_create_only(self):
        h = ArmHarness(self)
        ctx = h.context("h1-q01")
        record = judge_arm(h.deps, h.holdout, ctx, "facts_retry", run_arm("facts_retry", ctx, h.deps))
        path = write_verdict(h.campaign.directory, record)
        self.assertEqual(path.name, "h1-q01.facts_retry.json")
        self.assertEqual(read_verdict(h.campaign.directory, "h1-q01", "facts_retry"), json.loads(json.dumps(record)))
        self.assertIsNone(read_verdict(h.campaign.directory, "h1-q01", "selective_originals"))
        with self.assertRaises(JournalError):
            write_verdict(h.campaign.directory, record)


if __name__ == "__main__":
    unittest.main()
