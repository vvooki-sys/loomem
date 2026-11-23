"""paid_arms: F/S/A over the miniature plan + holdout with FakeOpenAI on loopback.

``ArmHarness`` (not a TestCase) wires sheet, pinned plan + judge prompt,
verified holdout, campaign, journal, fake provider and ArmDeps; the judge and
report tests import it. No frozen prompt, no real holdout, no network.
"""
import hashlib
import itertools
import json
import os
from pathlib import Path
import tempfile
import unittest

from paid_arms import ARM_STAGES, ARMS, ArmDeps, ArmOutcome, read_arm_outcome, run_arm, stage_binding, write_arm_outcome
from paid_call_journal import CallJournal, JournalError
from paid_campaign import SublimitExceeded, open_campaign
from paid_context import build_context
from paid_fake_openai import FakeOpenAI
from paid_holdout import leaks_gold, verify_holdout
from paid_openai_wire import InputTokensBreach, Rejected4xx, RunStopped, Transport
from paid_prompts import STAGES, load_prompts
from paid_test_support import load_fixture_sheet, make_campaign, write_holdout, write_judge_prompt, write_plan
from test_paid_context import sources_for, synthetic_search

MODELS = {"diagnostic": "gpt-6-luna", "audit": "gpt-6-luna", "selection": "gpt-6-luna",
          "answer": "gpt-6-luna", "judge": "gpt-6.2-astra"}


def system_sha(prompts, stage):
    return hashlib.sha256(prompts.stage(stage).system.encode("utf-8")).hexdigest()


class ArmHarness:
    def __init__(self, test, scenarios=None, unrouted=(), count_scenarios=None, **spec_overrides):
        self.tmp = tempfile.TemporaryDirectory()
        test.addCleanup(self.tmp.cleanup)
        root = Path(self.tmp.name)
        self.sheet = load_fixture_sheet(root)
        plan_path, plan_sha = write_plan(root)
        judge_path, judge_sha = write_judge_prompt(root, plan_sha)
        self.prompts = load_prompts(plan_path, plan_sha, judge_prompt=(judge_path, judge_sha))
        holdout_dir, manifest_sha, always_sha = write_holdout(root)
        self.holdout = verify_holdout(holdout_dir, manifest_sha, always_sha)
        os.environ["PAID_TEST_KEY"] = "fixture-only-key"
        test.addCleanup(os.environ.pop, "PAID_TEST_KEY", None)
        self.campaign = make_campaign(root, self.sheet, **spec_overrides)
        self.journal = CallJournal(self.campaign.directory)
        roles = {system_sha(self.prompts, s): s for s in STAGES if s not in unrouted}
        self.fake = FakeOpenAI(scenarios or {}, model_by_role=MODELS, role_by_system_sha256=roles,
                               count_scenarios=count_scenarios).__enter__()
        test.addCleanup(self.fake.__exit__, None, None, None)
        self.transport = Transport(self.fake.url, "PAID_TEST_KEY", {"127.0.0.1"})
        self.clock = itertools.count(1_700_000_000).__next__
        self.deps = self._deps()

    def _deps(self, **extra):
        return ArmDeps(self.campaign, self.journal, self.sheet, self.prompts, self.transport,
                       self.holdout.sources_bytes_max(), self.holdout.manifest_sha256, clock=self.clock, **extra)

    def reopen(self):
        """Process restart: campaign (ledger) and journal re-read from disk; same provider."""
        self.campaign = open_campaign(self.campaign.directory, self.sheet)
        self.journal = CallJournal(self.campaign.directory)
        self.deps = self._deps()
        return self.deps

    def context(self, qid, **search_kw):
        view = self.holdout.execution_view(qid)
        history = self.holdout.histories[view["history_id"]]
        search = synthetic_search(history, **search_kw)
        return build_context(view, search, sources_for(history), self.sheet.caps(), zero_facts=False)

    def intents(self):
        return [(i["letter"], i["stage"], i["binding"]["arm"]) for _, i in sorted(self.journal.state.intents.items(),
                                                                               key=lambda kv: kv[1]["created_at"])]

    def roles(self):
        return [entry["role"] for entry in self.fake.log]

    def requests_by_stage(self):
        out = []
        for rid, intent in self.journal.state.intents.items():
            body = json.loads(self.journal.path(rid, "request").read_text())
            out.append((intent["stage"], json.dumps(body, ensure_ascii=False)))
        return out


def stripped(outcome):
    data = outcome.to_dict()
    for record in data["stages"].values():
        record.pop("replayed")
    return data


class ArmTests(unittest.TestCase):
    def test_f_is_exactly_two_luna_calls_with_letter_f(self):
        h = ArmHarness(self)
        ctx = h.context("h1-q01")
        outcome = run_arm("facts_retry", ctx, h.deps)
        self.assertEqual((outcome.status, outcome.answer), ("ok", "Synthetic answer."))
        self.assertEqual(h.intents(), [("F", "diagnostic", "facts_retry"), ("F", "answer", "facts_retry")])
        self.assertEqual(h.roles(), ["diagnostic", "answer"])
        self.assertEqual(tuple(outcome.stages), ARM_STAGES["facts_retry"])
        self.assertTrue(all(s["model"] == "gpt-6-luna" and s["outcome"] == "ok" for s in outcome.stages.values()))
        self.assertEqual((outcome.selected_ids, outcome.rejected_ids, outcome.need), ([], [], None))
        self.assertEqual(outcome.diagnostic["diagnostic"]["coverage"], "complete")
        for entry in h.fake.log:
            wire = entry["wire"]
            self.assertEqual((wire["reasoning_effort"], wire["model"]), ("low", "gpt-6-luna"))
            self.assertEqual(tuple(json.loads(wire["messages"][1]["content"])), h.prompts.stage(entry["role"]).user_keys)
        answer_user = json.loads(h.fake.log[1]["wire"]["messages"][1]["content"])
        self.assertEqual((answer_user["evidence"]["originals"], len(answer_user["evidence"]["facts"])), ([], 2))

    def test_s_always_calls_all_four_stages_and_records_need(self):
        h = ArmHarness(self)
        outcome = run_arm("selective_originals", h.context("h1-q01"), h.deps)
        self.assertEqual(outcome.status, "ok")
        self.assertEqual([s for _, s, _ in h.intents()], list(ARM_STAGES["selective_originals"]))
        self.assertEqual({letter for letter, _, _ in h.intents()}, {"S"})
        self.assertEqual(h.roles(), ["diagnostic", "audit", "selection", "answer"])
        self.assertIn("required", outcome.need)
        self.assertEqual(outcome.selected_ids, [r["record_id"] for r in h.context("h1-q01")["menu"]])
        answer_user = json.loads(h.fake.log[3]["wire"]["messages"][1]["content"])
        self.assertEqual([r["record_id"] for r in answer_user["evidence"]["originals"]], outcome.selected_ids)
        selection_user = json.loads(h.fake.log[2]["wire"]["messages"][1]["content"])
        self.assertEqual(set(selection_user), {"diagnostic", "facts", "original_records", "question"})

    def test_a_packs_all_menu_originals_in_rank_order(self):
        h = ArmHarness(self)
        qid = next(iter(h.holdout.always_ids))
        ctx = h.context(qid, extra_fragments=20)
        self.assertEqual(len(ctx["menu"]), 24)
        outcome = run_arm("always_originals", ctx, h.deps)
        self.assertEqual((outcome.status, [s for _, s, _ in h.intents()]), ("ok", ["diagnostic", "answer"]))
        self.assertEqual({letter for letter, _, _ in h.intents()}, {"A"})
        self.assertEqual(outcome.selected_ids, [r["record_id"] for r in ctx["menu"]])
        self.assertEqual(len(outcome.selected_ids), 24)

    def test_evidence_at_the_cap_is_exactly_what_the_prompt_sees(self):
        """Regression (pilot dry-run): the answer payload carried pack_evidence's bookkeeping keys and
        came out above EVIDENCE_CAP although the packer had measured the cap without them."""
        from paid_context import byte_size
        h = ArmHarness(self)
        qid = next(iter(h.holdout.always_ids))
        ctx = h.context(qid, extra_fragments=400)
        cap = h.sheet.caps()["EVIDENCE_CAP"]
        outcome = run_arm("always_originals", ctx, h.deps)
        self.assertEqual(outcome.status, "ok", outcome.error)
        self.assertTrue(outcome.rejected_ids, "the menu must overflow the evidence cap for this test")
        answer = [e for e in h.fake.log if e["role"] == "answer"][0]
        evidence = json.loads(answer["wire"]["messages"][1]["content"])["evidence"]
        self.assertEqual(set(evidence), {"facts", "originals"})
        self.assertLessEqual(byte_size(evidence), cap)
        self.assertGreater(byte_size(evidence), cap - 2048)

    def test_context_error_inside_an_arm_is_a_technical_error_not_a_stop(self):
        h = ArmHarness(self)
        qid = h.holdout.question_order[0]
        ctx = h.context(qid)
        ctx["facts"] = [{**ctx["facts"][0], "content": "x" * (h.sheet.caps()["FACTS_JSON_CAP"] + 1)}]
        outcome = run_arm("facts_retry", ctx, h.deps)
        self.assertEqual(outcome.status, "technical_error")
        self.assertIn("ContextError", outcome.error)
        self.assertEqual(h.intents(), [], "nothing is reserved for an over-cap context")

    def test_length_in_answer_means_status_length_and_later_stages_skipped(self):
        h = ArmHarness(self, scenarios={"answer": "length"})
        outcome = run_arm("facts_retry", h.context("h1-q01"), h.deps)
        self.assertEqual((outcome.status, outcome.answer), ("length", None))
        self.assertEqual(outcome.stages["answer"]["outcome"], "length")
        self.assertIn("finish_reason=length", outcome.error)
        self.assertEqual(h.roles(), ["diagnostic", "answer"])
        truncated = ArmHarness(self, scenarios={"diagnostic": "length"})
        outcome = run_arm("selective_originals", truncated.context("h1-q01"), truncated.deps)
        self.assertEqual((outcome.status, truncated.roles()), ("length", ["diagnostic"]))

    def test_unparsable_diagnostic_is_technical_error(self):
        h = ArmHarness(self, unrouted=("diagnostic",))  # the fake answers plain text for an unknown role
        outcome = run_arm("selective_originals", h.context("h1-q01"), h.deps)
        self.assertEqual(outcome.status, "technical_error")
        self.assertIn("not JSON", outcome.error)
        self.assertEqual(h.roles(), ["default"])
        self.assertEqual(outcome.stages["diagnostic"]["outcome"], "ok")
        self.assertIsNone(outcome.answer)

    def test_bindings_carry_context_and_prompt_hashes(self):
        h = ArmHarness(self)
        ctx = h.context("h1-q01")
        run_arm("facts_retry", ctx, h.deps)
        for intent in h.journal.state.intents.values():
            binding = intent["binding"]
            self.assertEqual(binding["context_sha256"], ctx["context_sha256"])
            self.assertEqual(binding["prompt_sha256"], h.prompts.stage(intent["stage"]).sha256)
            self.assertEqual(set(binding), {"history_id", "session_id", "question_id", "arm", "stage", "attempt",
                                            "manifest_sha256", "context_sha256", "prompt_sha256"})
            self.assertEqual((binding["manifest_sha256"], binding["question_id"]), (h.holdout.manifest_sha256, "h1-q01"))
        expected = stage_binding(h.deps, ctx, "facts_retry", "answer", h.prompts.stage("answer").sha256)
        self.assertIn(expected, [i["binding"] for i in h.journal.state.intents.values()])

    def test_no_gold_marker_in_any_non_judge_request(self):
        h = ArmHarness(self)
        for arm in ARMS:
            run_arm(arm, h.context("h1-q01"), h.deps)
        markers = h.holdout.gold_markers("h1-q01")
        self.assertTrue(markers)
        requests = h.requests_by_stage()
        self.assertEqual(len(requests), 8)
        for stage, text in requests:
            self.assertNotEqual(stage, "judge")
            self.assertEqual(leaks_gold(text, markers), [], f"gold leaked into a {stage} request")

    def test_resume_replays_without_new_transport_calls(self):
        h = ArmHarness(self)
        ctx = h.context("h1-q01")
        first = run_arm("facts_retry", ctx, h.deps)
        self.assertEqual(len(h.fake.log), 2)
        h.reopen()
        again = run_arm("facts_retry", ctx, h.deps)
        self.assertEqual(len(h.fake.log), 2)
        self.assertTrue(all(s["replayed"] for s in again.stages.values()))
        self.assertFalse(any(s["replayed"] for s in first.stages.values()))
        self.assertEqual(stripped(again), stripped(first))
        self.assertEqual(h.campaign.ledger.totals()["request_count"], 2)

    def test_fail_closed_stops_propagate(self):
        h = ArmHarness(self, scenarios={"answer": "bad_request_400"})
        with self.assertRaises(Rejected4xx):
            run_arm("facts_retry", h.context("h1-q01"), h.deps)
        blocked = ArmHarness(self, scenarios={"diagnostic": "http500"})
        with self.assertRaises(RunStopped):
            run_arm("facts_retry", blocked.context("h1-q01"), blocked.deps)
        self.assertTrue(blocked.journal.state.has_uncertain)
        tiny = ArmHarness(self, sublimits_usd={"E": "10", "F": "0.000001", "S": "30", "A": "10", "R": "10", "K": "0.2374"})
        with self.assertRaises(SublimitExceeded):
            run_arm("facts_retry", tiny.context("h1-q01"), tiny.deps)
        self.assertEqual(tiny.fake.log, [])
        counted = ArmHarness(self, count_scenarios={"diagnostic": "count_over_bound"})
        counter = lambda wire: 10 ** 7  # noqa: E731 - stands in for paid_input_tokens
        with self.assertRaises(InputTokensBreach):
            run_arm("facts_retry", counted.context("h1-q01"), counted._deps(precount=counter))
        self.assertEqual((counted.fake.log, counted.journal.state.intents), ([], {}))

    def test_outcome_round_trips_and_is_written_create_only(self):
        h = ArmHarness(self)
        outcome = run_arm("selective_originals", h.context("h1-q01"), h.deps)
        data = json.loads(json.dumps(outcome.to_dict()))
        self.assertEqual(ArmOutcome.from_dict(data), outcome)
        path = write_arm_outcome(h.campaign.directory, outcome)
        self.assertEqual(path, Path(h.campaign.directory) / "arms" / "h1-q01.selective_originals.json")
        self.assertEqual(read_arm_outcome(h.campaign.directory, "h1-q01", "selective_originals"), outcome)
        self.assertIsNone(read_arm_outcome(h.campaign.directory, "h1-q02", "selective_originals"))
        with self.assertRaises(JournalError):
            write_arm_outcome(h.campaign.directory, outcome)
        with self.assertRaises(ValueError):
            ArmOutcome.from_dict({**data, "status": "weird"})
        budget = ArmOutcome("h1-q02", "facts_retry", "not_started_budget", error="sublimit F")
        self.assertEqual(ArmOutcome.from_dict(budget.to_dict()), budget)


if __name__ == "__main__":
    unittest.main()
