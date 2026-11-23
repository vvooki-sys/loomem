"""CLI, dry-run on the miniature holdout (fake engine, real proxy/arms/judge/report), resume, acknowledgement."""
from decimal import Decimal
import io
import itertools
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest import mock

from paid_call_journal import CallJournal, request_id, sha256_hex
from paid_campaign import open_campaign
from paid_context import context_sha256
from paid_dry_run import KEY_ENV, fake_provider
from paid_openai_wire import build_wire, wire_bytes
from paid_runner import main as _main
from paid_test_support import PROFILES, binding, messages
from test_paid_preflight import build_fixture, runner_env, tokenizer_available

SANDBOX_PRESENT = Path("/usr/bin/sandbox-exec").is_file()


def main(argv, env=None):
    """paid_runner.main with stdout and stderr captured; returns (code, combined text)."""
    out, err = io.StringIO(), io.StringIO()
    with mock.patch("sys.stderr", err):  # refusals and phase progress lines go to stderr
        code = _main(argv, stdout=out, env=env, clock=itertools.count(1_700_000_000).__next__)
    return code, out.getvalue() + err.getvalue()


class CreateCampaignTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.fixture = build_fixture(self.root)
        self.campaign_dir = self.root / "campaign"

    def test_missing_limit_is_exit_2(self):
        code, _ = main(["create-campaign", *self.fixture.flags(self.campaign_dir, limit=None)])
        self.assertEqual(code, 2)
        self.assertFalse(self.campaign_dir.exists())

    def test_create_writes_the_extra_sections(self):
        code, out = main(["create-campaign", *self.fixture.flags(self.campaign_dir, extra=["--max-transcript-tokens", "30000"])])
        self.assertEqual(code, 0, out)
        spec = json.loads((self.campaign_dir / "campaign.json").read_text())
        self.assertEqual(spec["mode"], "paid")
        self.assertEqual(spec["extraction"], {"max_completion_tokens": 16384, "input_chunk_tokens": 4000,
                                              "split_on_truncation": True, "max_facts_per_chunk": 20,
                                              "completion_tokens_floor": 0, "max_transcript_tokens": 30000})
        self.assertEqual(set(spec["engine"]), {"binary", "binary_sha256", "template", "template_sha256", "model_dir",
                                               "model_onnx_sha256", "tokenizer_sha256", "sandbox_profile_sha256",
                                               "effective_config_sha256"})
        self.assertEqual(set(spec["plan_json"]["stage_sha256"]), {"diagnostic", "audit", "selection", "answer", "judge"})
        self.assertEqual(spec["judge_prompt"]["sha256"], self.fixture.judge_sha)
        self.assertEqual(spec["search"], {"top_k": 60, "reference_time_from_question_date": True})
        self.assertEqual(spec["input_tokens"]["enabled"], True)
        self.assertEqual(spec["reasoning_effort_source"]["checked_on"], "2026-11-22")
        self.assertIn("paid_runner.py", spec["harness_sha256"])
        self.assertIn("budget.py", spec["harness_sha256"])
        self.assertEqual(spec["provider"], {"base_url": "https://api.openai.com/v1", "api_key_env": "OPENAI_API_KEY",
                                            "allowed_host": "api.openai.com"})
        self.assertTrue((self.campaign_dir / "budget.json").is_file())
        code, _ = main(["create-campaign", *self.fixture.flags(self.campaign_dir)])
        self.assertEqual(code, 2)  # create-only
        code, _ = main(["create-campaign", *self.fixture.flags(self.root / "x", extra=["--binary-sha256", "0" * 64])])
        self.assertEqual(code, 2)  # a given engine hash must agree with the file

    def test_run_refuses_a_dry_run_campaign(self):
        code, _ = main(["create-campaign", "--mode", "dry_run", *self.fixture.flags(self.campaign_dir)])
        self.assertEqual(code, 0)
        code, out = main(["run", "--campaign-dir", str(self.campaign_dir)], env=runner_env(OPENAI_API_KEY="k"))
        self.assertEqual(code, 2)
        self.assertIn("paid run is refused", out)
        self.assertFalse((self.campaign_dir / "calls").exists() and any((self.campaign_dir / "calls").iterdir()))


class DryRunTests(unittest.TestCase):
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

    def dry_run(self, *extra):
        return main(["dry-run", *self.fixture.flags(self.campaign_dir), "--fake-engine", *extra], env=self.env)

    def summary(self):
        return json.loads((self.campaign_dir / "dry_run_summary.json").read_text())

    def test_full_dry_run_then_resume_makes_no_new_calls(self):
        code, out = self.dry_run()
        self.assertEqual(code, 0, out)
        self.assertIn("PREDICTED MAXIMUM", out)
        self.assertIn("USD p1", out)
        self.assertIn("USD p2", out)
        d = self.campaign_dir
        self.assertEqual(sorted(p.name for p in (d / "contexts").glob("*.json")), ["h1-q01.json", "h1-q02.json"])
        arms = sorted(p.name for p in (d / "arms").glob("*.json"))
        self.assertEqual(arms, ["h1-q01.always_originals.json", "h1-q01.facts_retry.json", "h1-q01.selective_originals.json",
                                "h1-q02.facts_retry.json", "h1-q02.selective_originals.json"])
        self.assertEqual(sorted(p.name for p in (d / "verdicts").glob("*.json")), arms)
        self.assertTrue((d / "reports" / "report-1.json").is_file())
        self.assertTrue((d / "extraction" / "h1.json").is_file())
        self.assertTrue((d / "extraction_system.json").is_file())
        self.assertTrue((d / "budget.json").is_file())
        summary = self.summary()
        self.assertIsNone(summary["stop"])
        self.assertTrue(all(c["passed"] for c in summary["preflight"]), summary["preflight"])
        # observed: 1 extraction request per session (fake engine: one chunk), control 1, S = 4 luna + 1 judge
        per_session = summary["observed"]["extraction_requests_per_session"]
        self.assertEqual([s["observed_requests"] for s in per_session], [1, 1])
        self.assertEqual([s["predicted_max_requests"] for s in per_session], [5, 5])
        self.assertTrue(all(s["o200k_tokens"] > 0 for s in per_session))
        self.assertEqual(summary["observed"]["requests_by_role_this_invocation"],
                         {"extraction": 2, "control": 1, "diagnostic": 5, "audit": 2, "selection": 2, "answer": 5, "judge": 5})
        self.assertEqual(summary["observed"]["journal_calls_by_stage"]["judge"], 5)
        self.assertEqual(summary["observed"]["input_token_probes_recorded"], 2 + 1 + 5 + 2 + 2 + 5 + 5)
        s_arm = json.loads((d / "arms" / "h1-q02.selective_originals.json").read_text())
        self.assertEqual(sorted(s_arm["stages"]), ["answer", "audit", "diagnostic", "selection"])
        self.assertEqual(s_arm["status"], "ok")
        self.assertIsNotNone(summary["observed"]["system_fingerprints"]["extraction"])
        self.assertEqual(summary["observed"]["pins"], {"gpt-6-luna": "gpt-6-luna", "gpt-6.2-astra": "gpt-6.2-astra"})
        maximum = summary["predicted_maximum"]
        self.assertEqual(maximum["policy_chosen"], "p1")
        self.assertEqual({r["letter"] for r in maximum["rows"]}, {"F", "S", "A", "R"})
        self.assertEqual(len(maximum["extraction_sessions"]), 2)
        self.assertEqual(maximum["extraction_sessions"][0]["max_requests"], 5)
        judge = next(r for r in maximum["rows"] if r["letter"] == "F" and r["stage"] == "judge")
        self.assertEqual(judge["calls"], 2)
        self.assertEqual(set(judge["usd_max"]), {"p1", "p2"})
        self.assertNotEqual(judge["usd_max"]["p1"], "unpriced")
        self.assertEqual(maximum["sublimits_sum"], "90")
        self.assertEqual(maximum["letters_over_sublimit"]["p1"], [])
        per_arm = summary["report"]["per_arm"]
        self.assertEqual((per_arm["facts_retry"]["expected"], per_arm["always_originals"]["expected"]), (2, 1))
        self.assertEqual(per_arm["selective_originals"]["correct"], 2)
        self.assertEqual(summary["report"]["money"]["by_letter"]["R"]["calls"], 1)
        self.assertLessEqual(Decimal(summary["report"]["money"]["by_letter"]["R"]["reserved_max_usd"]), Decimal("0.24"))
        self.assertEqual(json.loads((d / "campaign.json").read_text())["provider"]["api_key_env"], KEY_ENV)
        before = open_campaign(d).ledger.totals()
        # second invocation with --resume: every artifact exists, every request replays -> zero transport calls
        code, out = self.dry_run("--resume")
        self.assertEqual(code, 0, out)
        resumed = self.summary()
        self.assertTrue(resumed["resumed"])
        self.assertEqual(resumed["observed"]["requests_by_role_this_invocation"], {})
        self.assertEqual(resumed["observed"]["input_token_probes_this_invocation"], 0)
        self.assertEqual(open_campaign(d).ledger.totals(), before)
        self.assertTrue((d / "reports" / "report-2.json").is_file())
        self.assertNotIn(KEY_ENV, os.environ)

    def test_refused_with_a_live_key_in_the_environment(self):
        code, _ = main(["dry-run", *self.fixture.flags(self.campaign_dir), "--fake-engine"],
                       env=runner_env(OPENAI_API_KEY="k"))
        self.assertEqual(code, 2)
        self.assertFalse(self.campaign_dir.exists())

    def test_empty_facts_scenario_keeps_questions_in_the_denominator(self):
        code, out = self.dry_run("--scenario", "extraction=empty_facts")
        self.assertEqual(code, 0, out)
        summary = self.summary()
        self.assertEqual(len(summary["observed"]["zero_facts_events"]), 2)
        self.assertEqual(summary["report"]["extraction"]["zero_facts_sessions"], 2)
        self.assertEqual(summary["report"]["per_arm"]["facts_retry"]["zero_facts"], 2)
        self.assertEqual(summary["report"]["per_arm"]["facts_retry"]["expected"], 2)
        self.assertEqual(len(list((self.campaign_dir / "verdicts").glob("*.json"))), 5)

    def test_control_rejection_stops_before_any_other_call(self):
        code, out = self.dry_run("--scenario", "control=bad_request_400")
        self.assertEqual(code, 3, out)
        summary = self.summary()
        self.assertIn("Rejected4xx", summary["stop"])
        self.assertEqual(summary["observed"]["requests_by_role_this_invocation"], {"control": 1})
        self.assertFalse((self.campaign_dir / "arms").exists())
        money = summary["report"]["money"]["by_letter"]
        self.assertEqual((money["R"]["calls"], money["R"]["rejected_4xx"], money["R"]["actual_usd"]), (1, 1, "0"))
        self.assertEqual(sum(row["calls"] for letter, row in money.items() if letter != "R"), 0)


@unittest.skipUnless(SANDBOX_PRESENT, "sandbox-exec absent: paid-mode preflight cannot pass here")
class AcknowledgeUncertainTests(unittest.TestCase):
    """A paid campaign whose provider is the loopback fake: `run` is refused while a sent request is
    unsettled, and proceeds with the id acknowledged, marking that arm technical_error without a retry."""

    def setUp(self):
        if not tokenizer_available():
            self.skipTest("o200k cache unavailable (TIKTOKEN_CACHE_DIR)")
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.fixture = build_fixture(self.root)
        self.campaign_dir = self.root / "paid"
        patch = mock.patch("paid_preflight.MIN_FREE_BYTES", 1)
        patch.start()
        self.addCleanup(patch.stop)
        self.fake = fake_provider(self.fixture.prompts(), PROFILES, {})
        self.fake.__enter__()
        self.addCleanup(self.fake.__exit__, None, None, None)
        self.env = runner_env(OPENAI_API_KEY=self.fake.token)
        env_patch = mock.patch.dict(os.environ, {"OPENAI_API_KEY": self.fake.token})
        env_patch.start()
        self.addCleanup(env_patch.stop)
        code, out = main(["create-campaign", *self.fixture.flags(self.campaign_dir, extra=[
            "--provider-base-url", self.fake.url, "--api-key-env", "OPENAI_API_KEY"])], env=self.env)
        self.assertEqual(code, 0, out)
        self.sheet = self.fixture.sheet()
        self.campaign = open_campaign(self.campaign_dir, self.sheet)
        self.journal = CallJournal(self.campaign_dir)
        self.rid = self.forge_sent_request()
        self.write_contexts()

    def forge_sent_request(self):
        """intent + sent for q01/facts_retry/diagnostic, never settled (crash mid-request)."""
        wire = build_wire(self.campaign.profile("diagnostic"), messages())
        bind = binding(qid="h1-q01", arm="facts_retry", stage="diagnostic")
        bind["manifest_sha256"] = self.campaign.spec["holdout"]["manifest_sha256"]
        rid = request_id(self.campaign.campaign_id, bind, sha256_hex(wire_bytes(wire)))
        bound = self.sheet.stage_profile("diagnostic", 1000, True)
        max_usd = str(self.sheet.max_usd(bound, "p1"))
        token = self.campaign.ledger.reserve(rid, max_usd)
        clock = itertools.count(1_600_000_000).__next__
        self.journal.intent({"schema": 1, "request_id": rid, "letter": "F", "stage": "diagnostic", "binding": bind,
                             "max_usd": max_usd, "wire_sha256": sha256_hex(wire_bytes(wire)), "ledger_token": token,
                             "profile": self.campaign.profile("diagnostic"), "created_at": clock(),
                             "r_tokens": bound.r_tokens, "o_tokens": bound.o_tokens}, wire)
        self.journal.sent(rid, clock(), 5, "127.0.0.1")
        return rid

    def write_contexts(self):
        from paid_call_journal import write_create_only
        from paid_holdout import verify_holdout
        pin = self.campaign.spec["holdout"]
        holdout = verify_holdout(pin["dir"], pin["manifest_sha256"], pin["always50_sha256"])
        for qid in holdout.question_order:
            context = {**holdout.execution_view(qid), "facts": [], "menu": [], "source_ids": [], "zero_facts": True}
            context["context_sha256"] = context_sha256(context)
            write_create_only(self.campaign_dir / "contexts" / f"{qid}.json", context)

    def test_refused_then_acknowledged(self):
        code, out = main(["run", "--campaign-dir", str(self.campaign_dir), "--phase", "arms"], env=self.env)
        self.assertEqual(code, 2, out)
        self.assertIn("not acknowledged", out)
        self.assertEqual(self.fake.log, [])
        code, out = main(["run", "--campaign-dir", str(self.campaign_dir), "--phase", "arms",
                          "--acknowledge-uncertain", "not-a-known-rid"], env=self.env)
        self.assertEqual(code, 2)
        self.assertIn("unknown request ids", out)
        code, out = main(["run", "--campaign-dir", str(self.campaign_dir), "--phase", "arms",
                          "--acknowledge-uncertain", self.rid], env=self.env)
        self.assertEqual(code, 0, out)
        self.assertIn("control call", out)
        acked = json.loads((self.campaign_dir / "arms" / "h1-q01.facts_retry.json").read_text())
        self.assertEqual(acked["status"], "technical_error")
        self.assertEqual(acked["error"], f"acknowledged_uncertain:{self.rid}")
        self.assertEqual(acked["stages"], {})
        others = sorted(p.name for p in (self.campaign_dir / "arms").glob("*.json"))
        self.assertEqual(len(others), 5)
        for name in others:
            if name != "h1-q01.facts_retry.json":
                self.assertEqual(json.loads((self.campaign_dir / "arms" / name).read_text())["status"], "ok", name)
        journal = CallJournal(self.campaign_dir)
        f_q01 = [i for i in journal.state.intents.values()
                 if i["binding"].get("question_id") == "h1-q01" and i["binding"].get("arm") == "facts_retry"]
        self.assertEqual([i["request_id"] for i in f_q01], [self.rid])  # never retried
        self.assertEqual(journal.state.blocking(), [self.rid])  # still unsettled, still blocking without the flag
        roles = sorted(e["role"] for e in self.fake.log)
        self.assertEqual(roles.count("control"), 1)
        self.assertEqual(roles.count("judge"), 0)
        self.assertFalse((self.campaign_dir / "run.lock").exists())
        self.assertTrue((self.campaign_dir / "reports" / "report-1.json").is_file())


if __name__ == "__main__":
    unittest.main()
