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
from paid_fake_openai import INSUFFICIENT_QUOTA_429
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
        self.assertEqual(summary["observed"]["extraction_capped_events"], [])
        self.assertEqual((summary["report"]["extraction"]["capped_sessions"],
                          summary["report"]["extraction"]["discarded_facts_total"]), (0, 0))
        maximum = summary["predicted_maximum"]
        self.assertEqual(maximum["policy_chosen"], "p1")
        self.assertEqual({r["letter"] for r in maximum["rows"]}, {"F", "S", "A", "K"})
        self.assertEqual(len(maximum["extraction_sessions"]), 2)
        self.assertEqual(maximum["extraction_sessions"][0]["max_requests"], 5)
        judge = next(r for r in maximum["rows"] if r["letter"] == "F" and r["stage"] == "judge")
        self.assertEqual(judge["calls"], 2)
        self.assertEqual(set(judge["usd_max"]), {"p1", "p2"})
        self.assertNotEqual(judge["usd_max"]["p1"], "unpriced")
        self.assertEqual(maximum["sublimits_sum"], "90.2374")
        self.assertEqual(maximum["letters_over_sublimit"]["p1"], [])
        per_arm = summary["report"]["per_arm"]
        self.assertEqual((per_arm["facts_retry"]["expected"], per_arm["always_originals"]["expected"]), (2, 1))
        self.assertEqual(per_arm["selective_originals"]["correct"], 2)
        self.assertEqual(summary["report"]["money"]["by_letter"]["K"]["calls"], 1)
        self.assertLessEqual(Decimal(summary["report"]["money"]["by_letter"]["K"]["reserved_max_usd"]), Decimal("0.24"))
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

    def test_fact_limit_capped_extraction_is_a_counted_state_not_a_stop(self):
        code, out = self.dry_run("--scenario", "extraction=capped")
        self.assertEqual(code, 0, out)
        summary = self.summary()
        self.assertIsNone(summary["stop"])
        self.assertEqual(summary["observed"]["extraction_capped_events"],
                         ["h1.h1-s1.extraction_capped.json", "h1.h1-s2.extraction_capped.json"])
        self.assertEqual(summary["observed"]["zero_facts_events"], [])
        extraction = summary["report"]["extraction"]
        self.assertEqual((extraction["capped_sessions"], extraction["discarded_facts_total"],
                          extraction["zero_facts_sessions"]), (2, 4, 0))
        stored = json.loads((self.campaign_dir / "extraction" / "h1.json").read_text())
        self.assertEqual([(s["capped"], s["discarded_facts"], len(s["fact_ids"])) for s in stored["sessions"]],
                         [(True, 2, 20), (True, 2, 20)])
        event = json.loads((self.campaign_dir / "events" / "h1.h1-s1.extraction_capped.json").read_text())
        self.assertEqual((event["type"], event["discarded_facts"], event["requests"]),
                         ("extraction_capped", 2, stored["sessions"][0]["requests"]))
        report = json.loads((self.campaign_dir / "reports" / "report-1.json").read_text())
        self.assertEqual((report["capped_questions"], report["zero_facts_events"]), (["h1-q01", "h1-q02"], 0))
        per_arm = summary["report"]["per_arm"]
        self.assertEqual((per_arm["facts_retry"]["expected"], per_arm["always_originals"]["expected"]), (2, 1))
        self.assertEqual(per_arm["facts_retry"]["zero_facts"], 0)
        self.assertEqual(len(list((self.campaign_dir / "verdicts").glob("*.json"))), 5)

    def test_output_limit_400_on_one_selection_is_an_arm_length_not_a_stop(self):
        code, out = self.dry_run("--scenario", "selection=length_http400,ok")
        self.assertEqual(code, 0, out)
        summary = self.summary()
        self.assertIsNone(summary["stop"])
        self.assertEqual(summary["observed"]["requests_by_role_this_invocation"]["selection"], 2)
        statuses = {p.name: json.loads(p.read_text())["status"] for p in (self.campaign_dir / "arms").glob("*.json")}
        self.assertEqual(sorted(statuses.values()).count("length"), 1)
        self.assertEqual([n for n, s in statuses.items() if s == "length"][0].split(".")[1], "selective_originals")
        self.assertEqual(len(list((self.campaign_dir / "verdicts").glob("*.json"))), 5)
        report = summary["report"]
        self.assertEqual(report["per_arm"]["selective_originals"]["length"], 1)
        self.assertEqual(report["per_arm"]["selective_originals"]["expected"], 2)
        selection = report["money"]["by_stage"]["selection"]
        self.assertEqual((selection["calls"], selection["length"], selection["length_http400"], selection["rejected_4xx"]),
                         (2, 1, 1, 0))
        self.assertEqual(report["money"]["by_letter"]["S"]["length_http400"], 1)
        code, out = self.dry_run("--resume", "--scenario", "selection=length_http400,ok")
        self.assertEqual(code, 0, out)
        self.assertEqual(self.summary()["observed"]["requests_by_role_this_invocation"], {})

    def test_capped_extraction_with_a_failed_attempt_stops(self):
        code, out = self.dry_run("--scenario", "extraction=capped_failed_attempt")
        self.assertEqual(code, 3, out)
        summary = self.summary()
        self.assertIn("EngineError", summary["stop"])
        self.assertEqual(summary["observed"]["extraction_capped_events"], [])
        self.assertFalse((self.campaign_dir / "extraction" / "h1.json").exists())
        self.assertFalse((self.campaign_dir / "arms").exists())

    def test_control_rejection_stops_before_any_other_call(self):
        code, out = self.dry_run("--scenario", "control=bad_request_400")
        self.assertEqual(code, 3, out)
        summary = self.summary()
        self.assertIn("Rejected4xx", summary["stop"])
        self.assertEqual(summary["observed"]["requests_by_role_this_invocation"], {"control": 1})
        self.assertFalse((self.campaign_dir / "arms").exists())
        money = summary["report"]["money"]["by_letter"]
        self.assertEqual((money["K"]["calls"], money["K"]["rejected_4xx"], money["K"]["actual_usd"]), (1, 1, "0"))
        self.assertEqual(sum(row["calls"] for letter, row in money.items() if letter != "K"), 0)

    def rejected_ids(self):
        state = CallJournal(self.campaign_dir).state
        return sorted(r for r, s in state.settled.items() if s["outcome"] == "rejected_4xx")

    def test_rejected_judge_is_sent_again_only_with_retry_rejected(self):
        code, out = self.dry_run("--scenario", "judge=rejected_4xx,ok")
        self.assertEqual(code, 3, out)
        self.assertIn("Rejected4xx", self.summary()["stop"])
        [rid] = self.rejected_ids()
        ok_rid = next(r for r, s in CallJournal(self.campaign_dir).state.settled.items() if s["outcome"] == "ok")
        refusals = {("--retry-rejected", "not-a-known-rid"): "unknown request ids",
                    ("--retry-rejected", ok_rid): "not settled as rejected_4xx"}
        for flags, message in refusals.items():
            with self.subTest(flags=flags):
                code, out = self.dry_run("--resume", *flags)
                self.assertEqual(code, 2, out)
                self.assertIn(message, out)
        code, out = self.dry_run("--retry-rejected", rid)  # never on a new campaign
        self.assertEqual(code, 2, out)
        self.assertIn("needs --resume", out)
        code, out = self.dry_run("--resume")  # without the flag the replayed rejection still stops
        self.assertEqual(code, 3, out)
        self.assertEqual(self.summary()["observed"]["requests_by_role_this_invocation"], {})
        code, out = self.dry_run("--resume", "--retry-rejected", rid)
        self.assertEqual(code, 0, out)
        summary = self.summary()
        self.assertEqual((summary["stop"], summary["retry_rejected"]), (None, [rid]))
        self.assertEqual(summary["observed"]["requests_by_role_this_invocation"], {"judge": 5})
        judge = summary["report"]["money"]["by_stage"]["judge"]
        self.assertEqual((judge["calls"], judge["rejected_4xx"], judge["retried"], judge["max_attempt"], judge["settled"]),
                         (6, 1, 1, 2, 6))
        self.assertEqual(sum(row["judged"] for row in summary["report"]["per_arm"].values()), 5)
        self.assertEqual(sum(row["pending"] + row["ok_unjudged"] for row in summary["report"]["per_arm"].values()), 0)
        self.assertEqual(summary["report"]["money"]["by_stage"]["diagnostic"]["max_attempt"], 1)
        code, out = self.dry_run("--resume")  # later resume without the flag: attempt 2 replays, nothing is sent
        self.assertEqual(code, 0, out)
        resumed = self.summary()
        self.assertEqual(resumed["observed"]["requests_by_role_this_invocation"], {})
        self.assertEqual(resumed["observed"]["input_token_probes_this_invocation"], 0)
        self.assertEqual(resumed["report"]["money"]["by_stage"]["judge"]["retried"], 1)

    def test_budget_stop_on_a_retried_first_stage_is_not_started_budget(self):
        from paid_campaign import Campaign, SublimitExceeded
        code, out = self.dry_run("--scenario", "diagnostic=rejected_4xx,ok")  # h1-q01 facts_retry diagnostic
        self.assertEqual(code, 3, out)
        [rid] = self.rejected_ids()
        original = Campaign.check_sublimit

        def no_room_for_f(campaign, letter, committed, add):
            if letter == "F":
                raise SublimitExceeded("synthetic: sublimit F exhausted")
            return original(campaign, letter, committed, add)

        with mock.patch.object(Campaign, "check_sublimit", no_room_for_f):
            code, out = self.dry_run("--resume", "--retry-rejected", rid)
        self.assertEqual(code, 3, out)
        arm = json.loads((self.campaign_dir / "arms" / "h1-q01.facts_retry.json").read_text())
        self.assertEqual(arm["status"], "not_started_budget", "a 0 USD rejected_4xx is not a reservation")

    def test_retry_rejected_covers_control_extraction_and_arm_stages(self):
        for role, stage in (("control", "control"), ("extraction", "extraction"), ("diagnostic", "diagnostic")):
            with self.subTest(role=role):
                self.campaign_dir = self.root / f"dry-{role}"
                code, out = self.dry_run("--scenario", f"{role}=rejected_4xx,ok")
                self.assertEqual(code, 3, out)
                [rid] = self.rejected_ids()
                code, out = self.dry_run("--resume", "--retry-rejected", rid)
                self.assertEqual(code, 0, out)
                summary = self.summary()
                row = summary["report"]["money"]["by_stage"][stage]
                self.assertEqual((row["rejected_4xx"], row["retried"], row["max_attempt"]), (1, 1, 2))
                self.assertEqual(len(list((self.campaign_dir / "verdicts").glob("*.json"))), 5)
                code, out = self.dry_run("--resume")
                self.assertEqual(code, 0, out)
                self.assertEqual(self.summary()["observed"]["requests_by_role_this_invocation"], {})


@unittest.skipUnless(SANDBOX_PRESENT, "sandbox-exec absent: paid-mode preflight cannot pass here")
class PinnedFingerprintTests(unittest.TestCase):
    """A paid campaign pinned with --extraction-system-sha256: the proxy compares the engine's very
    first extraction prompt with the pin and halts BEFORE any reservation when it differs."""

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
        probe = mock.patch("paid_preflight_engine.probe_engine",
                           return_value={"passed": True, "problems": [], "observed": {
                               "system_bytes": 1, "wire_overhead_bytes": 1, "sheet_system_extraction": 6000,
                               "system_sha256": "0" * 64, "readiness_bad_body_status": 422}})
        probe.start()
        self.addCleanup(probe.stop)
        self.fake = fake_provider(self.fixture.prompts(), PROFILES, {})
        self.fake.__enter__()
        self.addCleanup(self.fake.__exit__, None, None, None)
        self.env = runner_env(OPENAI_API_KEY=self.fake.token)
        env_patch = mock.patch.dict(os.environ, {"OPENAI_API_KEY": self.fake.token})
        env_patch.start()
        self.addCleanup(env_patch.stop)

    def test_other_extraction_prompt_halts_before_any_reservation(self):
        from paid_fake_engine import fake_engine_factory
        code, out = main(["create-campaign", *self.fixture.flags(self.campaign_dir, extra=[
            "--provider-base-url", self.fake.url, "--api-key-env", "OPENAI_API_KEY",
            "--extraction-system-sha256", "1" * 64])], env=self.env)
        self.assertEqual(code, 0, out)
        err = io.StringIO()
        with mock.patch("sys.stderr", err):
            code = _main(["run", "--campaign-dir", str(self.campaign_dir), "--phase", "extraction"],
                         factories={"engine": fake_engine_factory}, stdout=io.StringIO(), env=self.env)
        self.assertEqual(code, 3, err.getvalue())
        self.assertIn("non-conforming extraction request", err.getvalue())
        journal = CallJournal(self.campaign_dir)
        letters = sorted(i["letter"] for i in journal.state.intents.values())
        self.assertEqual(letters, ["K"], "only the control call was reserved; no E reservation")
        ledger = json.loads((self.campaign_dir / "budget.json").read_text())
        self.assertEqual(len(ledger["requests"]), 1)
        self.assertEqual(len(self.fake.log), 1, "the engine's request never reached the provider")
        self.assertFalse((self.campaign_dir / "extraction" / "h1.json").exists())


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
        # Preflight #16 would launch the fixture's stand-in binary; the live probe has its own tests.
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
        for rid, message in (("not-a-known-rid", "unknown request ids"),
                             (self.rid, "not settled as rejected_4xx")):  # an unsettled request is not a rejection
            code, out = main(["run", "--campaign-dir", str(self.campaign_dir), "--phase", "arms",
                              "--retry-rejected", rid], env=self.env)
            self.assertEqual(code, 2, out)
            self.assertIn(message, out)
            self.assertNotIn("PASS", out, "refused before preflight")
        self.assertEqual(self.fake.log, [])
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

    def forge_rejected_control(self):
        """The control call K reserved, sent and settled rejected_4xx (HTTP 429), ledger settled at 0."""
        from paid_control import control_binding, control_messages, control_reservation
        wire = build_wire(self.campaign.profile("control"), control_messages())
        body, bind = wire_bytes(wire), control_binding(self.campaign)
        rid = request_id(self.campaign.campaign_id, bind, sha256_hex(body))
        bound, max_usd = control_reservation(self.campaign, self.sheet)
        token = self.campaign.ledger.reserve(rid, str(max_usd))
        clock = itertools.count(1_600_000_100).__next__
        self.journal.intent({"schema": 1, "request_id": rid, "campaign_id": self.campaign.campaign_id,
                             "created_at": clock(), "letter": "K", "stage": "control", "binding": bind,
                             "profile": {**self.campaign.profile("control"), "service_tier": "default"},
                             "bound_policy": "p1", "r_tokens": bound.r_tokens, "o_tokens": bound.o_tokens,
                             "max_usd": str(max_usd), "wire_sha256": sha256_hex(body), "wire_bytes": len(body),
                             "ledger_token": token}, wire)
        self.journal.sent(rid, clock(), 5, "127.0.0.1")
        error = json.dumps(INSUFFICIENT_QUOTA_429)
        self.journal.settled(rid, {"http_status": 429, "provider_request_id": None,
                                   "response_sha256": sha256_hex(error.encode("utf-8")), "settled_at": clock(),
                                   "model_observed": None, "usage": None, "finish_reason": None,
                                   "outcome": "rejected_4xx", "actual_usd": "0", "elapsed_ms": 1},
                             {"status": 429, "x_request_id": None, "body_text": error})
        self.campaign.ledger.settle(token, "0")
        return rid

    def test_paid_run_retries_a_rejected_request_once_and_then_replays_it(self):
        rid = self.forge_rejected_control()
        control = ["run", "--campaign-dir", str(self.campaign_dir), "--phase", "control",
                   "--acknowledge-uncertain", self.rid]  # the unrelated forged diagnostic stays acknowledged
        code, out = main([*control, "--retry-rejected", rid, "--acknowledge-uncertain", rid], env=self.env)
        self.assertEqual(code, 2, out)
        self.assertIn("name the same request ids", out)
        self.assertNotIn("PASS", out, "refused before preflight")
        code, out = main(control, env=self.env)  # without the flag the rejected K replays as a stop
        self.assertEqual(code, 3, out)
        self.assertIn(f"pass --retry-rejected {rid}", out)
        self.assertEqual(self.fake.log, [])
        code, out = main([*control, "--retry-rejected", rid], env=self.env)
        self.assertEqual(code, 0, out)
        self.assertEqual([e["role"] for e in self.fake.log], ["control"])
        self.assertIn(f"retry-rejected: sent again ['{rid[:12]}']; journaled unsent []; not reached []", out)
        journal = CallJournal(self.campaign_dir)
        [retried] = [r for r, i in journal.state.intents.items() if i["stage"] == "control" and r != rid]
        self.assertEqual(journal.state.intents[retried]["binding"], {**journal.state.intents[rid]["binding"], "attempt": 2})
        self.assertEqual((journal.state.settled[rid]["outcome"], journal.state.settled[retried]["outcome"]),
                         ("rejected_4xx", "ok"))
        code, out = main(control, env=self.env)  # resume without the flag: nothing new leaves
        self.assertEqual(code, 0, out)
        self.assertEqual(len(self.fake.log), 1)
        from paid_control import control_call
        from paid_openai_wire import Transport
        campaign = open_campaign(self.campaign_dir, self.sheet)
        again = control_call(campaign, CallJournal(self.campaign_dir), self.sheet, Transport.from_campaign(campaign),
                             acknowledged=frozenset({self.rid}))
        self.assertEqual((again.request_id, again.replayed), (retried, True))  # attempt 1 -> 2 is followed
        self.assertEqual(len(self.fake.log), 1)

    def test_retry_names_ids_the_phase_did_not_reach(self):
        rid = self.forge_rejected_control()
        with mock.patch("paid_runner.has_settled_control", return_value=True):  # K not attempted this invocation
            code, out = main(["run", "--campaign-dir", str(self.campaign_dir), "--phase", "control",
                              "--acknowledge-uncertain", self.rid, "--retry-rejected", rid], env=self.env)
        self.assertEqual(code, 0, out)
        self.assertIn(f"retry-rejected: sent again []; journaled unsent []; not reached ['{rid[:12]}']", out)
        self.assertEqual(self.fake.log, [])

    def test_retry_whose_next_attempt_was_never_sent_is_journaled_unsent(self):
        """Crash window: the attempt-2 intent is written, then ``journal.sent`` fails; the run stops and
        the summary must not claim the request was sent again."""
        rid = self.forge_rejected_control()
        with mock.patch.object(CallJournal, "sent", side_effect=OSError("crash before sent")):
            code, out = main(["run", "--campaign-dir", str(self.campaign_dir), "--phase", "control",
                              "--acknowledge-uncertain", self.rid, "--retry-rejected", rid], env=self.env)
        self.assertEqual(code, 3, out)
        self.assertIn(f"retry-rejected: sent again []; journaled unsent ['{rid[:12]}']; not reached []", out)
        self.assertEqual(self.fake.log, [], "the provider was never called")
        state = CallJournal(self.campaign_dir).state
        [unsent] = [r for r, i in state.intents.items() if i["stage"] == "control" and r != rid]
        self.assertEqual((state.intents[unsent]["binding"]["attempt"], unsent in state.sent), (2, False))


@unittest.skipUnless(SANDBOX_PRESENT, "sandbox-exec absent: paid-mode preflight cannot pass here")
class ControlPhaseTests(unittest.TestCase):
    """`run --phase control` on a paid campaign whose provider is the loopback fake: only the control
    call K leaves, the report is written and the engine is never started; a rerun sends nothing."""

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

    def run_control(self):
        from paid_fake_engine import fake_engine_factory
        out, err = io.StringIO(), io.StringIO()
        with mock.patch("sys.stderr", err):
            code = _main(["run", "--campaign-dir", str(self.campaign_dir), "--phase", "control"],
                         factories={"engine": fake_engine_factory}, stdout=out, env=self.env,
                         clock=itertools.count(1_700_000_000).__next__)
        return code, out.getvalue() + err.getvalue()

    def test_control_sends_only_k_then_nothing_on_rerun(self):
        code, out = self.run_control()
        self.assertEqual(code, 0, out)
        self.assertIn("control call", out)
        journal = CallJournal(self.campaign_dir)
        self.assertEqual([i["letter"] for i in journal.state.intents.values()], ["K"])
        self.assertEqual(journal.state.blocking(), [])
        self.assertEqual([e["role"] for e in self.fake.log], ["control"])
        self.assertEqual(len(json.loads((self.campaign_dir / "budget.json").read_text())["requests"]), 1)
        for artifact in ("extraction", "contexts", "arms", "verdicts"):
            self.assertFalse((self.campaign_dir / artifact).exists(), artifact)
        self.assertTrue((self.campaign_dir / "reports" / "report-1.json").is_file())
        self.assertFalse((self.campaign_dir / "run.lock").exists())
        # a settled K is not repeated: the second invocation sends nothing and still writes a report
        code, out = self.run_control()
        self.assertEqual(code, 0, out)
        self.assertNotIn("control call", out)
        self.assertEqual(len(self.fake.log), 1)
        self.assertEqual(len(CallJournal(self.campaign_dir).state.intents), 1)
        self.assertTrue((self.campaign_dir / "reports" / "report-2.json").is_file())


if __name__ == "__main__":
    unittest.main()
