"""Preflight checks over a complete fixture campaign created through the real CLI flags."""
from dataclasses import dataclass
import io
import itertools
import json
import os
from pathlib import Path
import shutil
import tempfile
import unittest
from unittest import mock

from paid_call_journal import CallJournal, request_id, sha256_hex
from paid_campaign import create_campaign, open_campaign
from paid_openai_wire import build_wire, wire_bytes
from paid_preflight import all_passed, format_checks, run_preflight, tokenizer_or_none
from paid_preflight_probes import effective_config_check, sandbox_probe
from paid_price_sheet import load_sheet
from paid_prompts import load_prompts
from paid_runner import main as _main
from paid_runner_cli import build_parser, compose_spec
from paid_test_support import PROFILES, binding, messages, write_holdout, write_judge_prompt, write_plan, write_sheet

HERE = Path(__file__).resolve().parent
TEMPLATE = HERE.parents[1] / "config.toml"
SOURCE = "https://example.invalid/docs@2026-11-22"
SUBLIMITS = {"E": "10", "F": "30", "S": "30", "A": "10", "R": "10", "K": "0.2374"}


@dataclass
class Fixture:
    root: Path
    sheet_path: Path
    sheet_sha: str
    plan_path: Path
    plan_sha: str
    judge_path: Path
    judge_sha: str
    holdout: Path
    manifest_sha: str
    always_sha: str
    binary: Path
    template: Path
    model_dir: Path

    def flags(self, campaign_dir, *, limit="100", sublimits=None, judge=True, profiles=None, extra=()):
        args = ["--campaign-dir", str(campaign_dir)]
        if limit is not None:
            args += ["--limit-usd", limit]
        for letter, amount in (sublimits or SUBLIMITS).items():
            args += ["--sublimit", f"{letter}={amount}"]
        args += ["--bound-policy", "p1", "--accept-p1-framing",
                 "--cost-sheet", str(self.sheet_path), "--cost-sheet-sha256", self.sheet_sha,
                 "--plan-json", str(self.plan_path), "--plan-json-sha256", self.plan_sha,
                 "--holdout-dir", str(self.holdout), "--holdout-manifest-sha256", self.manifest_sha,
                 "--always50-sha256", self.always_sha, "--binary", str(self.binary), "--template", str(self.template),
                 "--model-dir", str(self.model_dir), "--reasoning-effort-source", SOURCE, "--tokenizer-source", SOURCE,
                 "--timeout-s", "5"]
        if judge:
            args += ["--judge-prompt", str(self.judge_path), "--judge-prompt-sha256", self.judge_sha]
        for stage, profile in (profiles or PROFILES).items():
            args += ["--profile", f"{stage}={profile['model']}:{profile['reasoning_effort']}:{profile['max_completion_tokens']}"]
        return args + list(extra)

    def sheet(self):
        return load_sheet(self.sheet_path, self.sheet_sha)

    def prompts(self, judge=True):
        return load_prompts(self.plan_path, self.plan_sha, (self.judge_path, self.judge_sha) if judge else None)

    def spec(self, campaign_dir, mode="paid", **kw):
        """A spec composed exactly as create-campaign would (for registries that bypass creation checks)."""
        args = build_parser().parse_args(["create-campaign", *self.flags(campaign_dir, **kw)])
        return compose_spec(args, self.prompts(kw.get("judge", True)), mode=mode, created_at="2026-11-22T00:00:00Z")


def build_fixture(root, **holdout_kw):
    root = Path(root)
    sheet_path, sheet_sha = write_sheet(root)
    plan_path, plan_sha = write_plan(root)
    judge_path, judge_sha = write_judge_prompt(root, plan_sha)
    holdout, manifest_sha, always_sha = write_holdout(root, **holdout_kw)
    engine = root / "engine-artifacts"
    (engine / "model").mkdir(parents=True)
    binary = engine / "loomem-server"
    binary.write_text("#!/bin/sh\nexit 0\n")
    binary.chmod(0o755)
    template = engine / "config.toml"
    shutil.copyfile(TEMPLATE, template)
    (engine / "model" / "model.onnx").write_bytes(b"not a real model")
    (engine / "model" / "tokenizer.json").write_bytes(b"{}")
    return Fixture(root, sheet_path, sheet_sha, plan_path, plan_sha, judge_path, judge_sha, holdout, manifest_sha,
                   always_sha, binary, template, engine / "model")


def main(argv, env=None):
    """paid_runner.main with stdout captured (keeps the test output clean); `env` never defaults
    to the machine environment, which may carry a real OPENAI_API_KEY."""
    return _main(argv, stdout=io.StringIO(), env=runner_env() if env is None else env)


def runner_env(**extra):
    env = {"PATH": os.environ.get("PATH", ""), "HOME": os.environ.get("HOME", "")}
    cache = os.environ.get("TIKTOKEN_CACHE_DIR")
    if cache:
        env["TIKTOKEN_CACHE_DIR"] = cache
    env.update(extra)
    return env


def tokenizer_available():
    return tokenizer_or_none(runner_env()) is not None


def by_number(checks):
    return {c.name.split()[0]: c for c in checks}


class PreflightTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.fixture = build_fixture(self.root)
        self.campaign_dir = self.root / "campaign"
        self.env = runner_env(OPENAI_API_KEY="fixture-not-a-real-key")

    def create(self, mode="paid", **kw):
        flags = self.fixture.flags(self.campaign_dir, **kw)
        return main(["create-campaign", "--mode", mode, *flags])

    def preflight(self, mode_command="preflight", env=None, **kw):
        kw.setdefault("engine_probe", False)
        return run_preflight(self.campaign_dir, mode_command=mode_command, env=self.env if env is None else env, **kw)

    def test_all_pass_for_a_complete_paid_campaign(self):
        if not tokenizer_available():
            self.skipTest("o200k cache unavailable (TIKTOKEN_CACHE_DIR)")
        self.assertEqual(self.create(), 0)
        with mock.patch("paid_preflight.MIN_FREE_BYTES", 1):  # the 2 GiB rule itself is checked below
            checks = self.preflight()
        self.assertEqual(len(checks), 17)
        self.assertTrue(all_passed(checks), format_checks(checks))
        with mock.patch("paid_preflight.MIN_FREE_BYTES", 2 ** 62):
            self.assertIn("free disk", by_number(self.preflight())["13"].detail)
        self.assertIn("skipped by caller", by_number(checks)["9"].detail)
        self.assertIn("TOML check skipped", by_number(checks)["10"].detail)
        with mock.patch("paid_preflight.MIN_FREE_BYTES", 1):
            self.assertEqual(main(["preflight", "--campaign-dir", str(self.campaign_dir), "--no-engine-probe"], env=self.env), 0)

    def test_registry_refusals(self):
        """Pins and limits rejected at creation (validate_spec) or by preflight #4/#5."""
        self.assertEqual(self.create(sublimits={"E": "10", "F": "30", "S": "30", "A": "10"}), 2)  # missing R
        self.assertFalse(self.campaign_dir.exists())
        spec = self.fixture.spec(self.campaign_dir)
        spec["profiles"]["judge"]["model"] = "gpt-6.9-unpriced"
        create_campaign(self.campaign_dir, spec)  # no sheet at creation: preflight must catch it
        checks = by_number(self.preflight())
        self.assertFalse(checks["4"].passed)
        self.assertIn("unpriced", checks["4"].detail)

    def test_sublimit_below_predicted_maximum(self):
        self.assertEqual(self.create(sublimits={**SUBLIMITS, "F": "0.01"}), 0)
        check = by_number(self.preflight())["5"]
        self.assertFalse(check.passed)
        self.assertIn("'F'", check.detail)

    def test_hash_mismatches(self):
        cases = {"2": ("holdout", "manifest_sha256"), "3": ("plan_json", "sha256"), "4": ("cost_sheet", "sha256")}
        for number, (section, key) in cases.items():
            with self.subTest(check=number):
                spec = self.fixture.spec(self.campaign_dir)
                spec[section][key] = "f" * 64
                directory = self.root / f"campaign-{number}"
                create_campaign(directory, spec)
                checks = by_number(run_preflight(directory, mode_command="preflight", env=self.env, engine_probe=False))
                self.assertFalse(checks[number].passed, checks[number].detail)
                self.assertIn("mismatch", checks[number].detail.lower())

    def test_sandbox_absent(self):
        self.assertEqual(self.create(), 0)
        with mock.patch("paid_preflight_probes.SANDBOX", str(self.root / "no-such-sandbox-exec")):
            paid = by_number(self.preflight(engine_probe=True))["9"]
            self.assertFalse(paid.passed)
            self.assertIn("absent", paid.detail)
        dry_dir = self.root / "dry"
        main(["create-campaign", "--mode", "dry_run", *self.fixture.flags(dry_dir, judge=False)])
        with mock.patch("paid_preflight_probes.SANDBOX", str(self.root / "no-such-sandbox-exec")):
            checks = by_number(run_preflight(dry_dir, mode_command="dry-run", engine_probe=True,
                                             env=runner_env(LOOMEM_DRY_RUN_PROVIDER_TOKEN="t")))
            self.assertTrue(checks["9"].passed)
            self.assertIn("skipped", checks["9"].detail)

    @unittest.skipUnless(Path("/usr/bin/sandbox-exec").is_file(), "sandbox-exec absent")
    def test_sandbox_probe_eperm(self):
        passed, detail = sandbox_probe()
        self.assertTrue(passed, detail)
        self.assertIn("EPERM", detail)
        self.assertIn("loopback reachable", detail)

    def test_effective_config_toml(self):
        spec = self.fixture.spec(self.campaign_dir)
        passed, detail = effective_config_check(spec["engine"], spec["extraction"])
        self.assertTrue(passed, detail)
        bad = dict(spec["engine"], effective_config_sha256="0" * 64)
        passed, detail = effective_config_check(bad, spec["extraction"])
        self.assertFalse(passed)
        self.assertIn("effective_config_sha256", detail)
        self.assertEqual(self.create(), 0)
        checks = by_number(self.preflight(engine_probe=True))
        self.assertTrue(checks["10"].passed, checks["10"].detail)
        self.assertIn("provider bench", checks["10"].detail)

    def test_effective_config_toml_with_engine_levers(self):
        lever = ["--engine-lever", "search.vector_fill_missing=true", "--engine-lever",
                 "knowledge_extraction.prompt_variant=relations", "--engine-lever", "search.state_followup_weight=1.0"]
        plain, spec = self.fixture.spec(self.campaign_dir), self.fixture.spec(self.campaign_dir, extra=lever)
        # recorded only when non-default (the weight at 1.0 is dropped), and the pin follows the levers
        self.assertEqual(spec["engine"]["levers"], {"search.vector_fill_missing": True,
                                                    "knowledge_extraction.prompt_variant": "relations"})
        self.assertNotIn("levers", plain["engine"])
        self.assertNotEqual(spec["engine"]["effective_config_sha256"], plain["engine"]["effective_config_sha256"])
        passed, detail = effective_config_check(spec["engine"], spec["extraction"])
        self.assertTrue(passed, detail)
        self.assertIn('"search.vector_fill_missing": true', detail)
        # the pinned hash of the lever-less config does not match a campaign that set levers (and vice versa)
        swapped = dict(spec["engine"], effective_config_sha256=plain["engine"]["effective_config_sha256"])
        passed, detail = effective_config_check(swapped, spec["extraction"])
        self.assertFalse(passed)
        self.assertIn("effective_config_sha256", detail)
        dropped = {k: v for k, v in spec["engine"].items() if k != "levers"}
        passed, detail = effective_config_check(dropped, spec["extraction"])
        self.assertFalse(passed)
        self.assertIn("effective_config_sha256", detail)
        # a lever the TOML renderer cannot honour is a FAIL, never a silent default
        passed, detail = effective_config_check(dict(spec["engine"], levers={"search.rerank_enabled": True}), spec["extraction"])
        self.assertFalse(passed)
        self.assertIn("unknown engine lever", detail)
        passed, detail = effective_config_check(dict(spec["engine"], levers={"search.vector_fill_missing": 1}), spec["extraction"])
        self.assertFalse(passed)
        # through the CLI: campaign.json carries the levers and preflight #10 passes on the created campaign
        self.assertEqual(self.create(extra=lever), 0)
        written = json.loads((self.campaign_dir / "campaign.json").read_text())
        self.assertEqual(written["engine"]["levers"], spec["engine"]["levers"])
        self.assertEqual(written["engine"]["effective_config_sha256"], spec["engine"]["effective_config_sha256"])
        checks = by_number(self.preflight(engine_probe=True))
        self.assertTrue(checks["10"].passed, checks["10"].detail)
        self.assertIn("engine levers", checks["10"].detail)

    def test_engine_lever_flag_refusals_and_default_shape(self):
        for extra in (["--engine-lever", "search.rerank_enabled=true"],                      # unknown key
                      ["--engine-lever", "search.vector_fill_missing=1"],                     # wrong type
                      ["--engine-lever", "knowledge_extraction.prompt_variant=relations_v9"],  # outside the enum
                      ["--engine-lever", "search.vector_fill_missing"],                       # no value
                      ["--engine-lever", "search.vector_fill_missing=true", "--engine-lever", "search.vector_fill_missing=false"]):
            with self.subTest(extra=extra):
                self.assertEqual(self.create(extra=extra), 2)
                self.assertFalse(self.campaign_dir.exists())
        # every lever explicitly at its default: campaign.json is the all-default one (no levers key, same pin)
        defaults = ["--engine-lever", "search.vector_fill_missing=false", "--engine-lever",
                    "knowledge_extraction.prompt_variant=default", "--engine-lever",
                    'search.state_followup_fact_types=["project_state", "event"]']
        plain = self.fixture.spec(self.campaign_dir)
        self.assertEqual(self.create(extra=defaults), 0)
        written = json.loads((self.campaign_dir / "campaign.json").read_text())
        self.assertNotIn("levers", written["engine"])
        self.assertEqual(written["engine"]["effective_config_sha256"], plain["engine"]["effective_config_sha256"])

    def test_credential_placement(self):
        self.assertEqual(self.create(), 0)
        key = self.env["OPENAI_API_KEY"]
        leaked = {"PATH": "/usr/bin", "OPENAI_API_KEY": key}
        self.assertFalse(by_number(self.preflight(engine_env=leaked))["8"].passed)
        renamed = {"PATH": "/usr/bin", "LOOMEM_AUTH_TOKEN": key}
        check = by_number(self.preflight(engine_env=renamed))["8"]
        self.assertFalse(check.passed)
        self.assertIn("LOOMEM_AUTH_TOKEN", check.detail)
        self.assertFalse(by_number(self.preflight(env=runner_env()))["8"].passed)  # key missing in paid mode
        self.assertTrue(by_number(self.preflight())["8"].passed)
        dry_dir = self.root / "dry"
        main(["create-campaign", "--mode", "dry_run", *self.fixture.flags(dry_dir, judge=False),
              "--api-key-env", "LOOMEM_DRY_RUN_PROVIDER_TOKEN"])
        present = runner_env(OPENAI_API_KEY=key, OPENAI_API_KEY_UNUSED="x", LOOMEM_DRY_RUN_PROVIDER_TOKEN="t")
        self.assertFalse(by_number(run_preflight(dry_dir, mode_command="dry-run", env=present, engine_probe=False))["8"].passed)
        absent = runner_env(LOOMEM_DRY_RUN_PROVIDER_TOKEN="t")
        self.assertTrue(by_number(run_preflight(dry_dir, mode_command="dry-run", env=absent, engine_probe=False))["8"].passed)

    def test_missing_sources_and_judge_prompt_and_control(self):
        spec = self.fixture.spec(self.campaign_dir, judge=False)
        del spec["reasoning_effort_source"]
        spec["profiles"].pop("control")
        create_campaign(self.campaign_dir, spec)
        checks = by_number(self.preflight())
        self.assertFalse(checks["7"].passed)
        self.assertIn("reasoning_effort_source", checks["7"].detail)
        self.assertFalse(checks["3"].passed)
        self.assertIn("judge_prompt", checks["3"].detail)
        self.assertFalse(checks["15"].passed)
        dry_dir = self.root / "dry"
        main(["create-campaign", "--mode", "dry_run", *self.fixture.flags(dry_dir, judge=False)])
        checks = by_number(run_preflight(dry_dir, mode_command="dry-run", engine_probe=False,
                                         env=runner_env(LOOMEM_DRY_RUN_PROVIDER_TOKEN="t")))
        self.assertTrue(checks["3"].passed, checks["3"].detail)

    def test_max_transcript_tokens_vs_o200k(self):
        self.assertEqual(self.create(extra=["--max-transcript-tokens", "1"]), 0)
        self.assertEqual(open_campaign(self.campaign_dir).spec["extraction"]["max_transcript_tokens"], 1)
        env = {k: v for k, v in self.env.items() if k != "TIKTOKEN_CACHE_DIR"}
        check = by_number(self.preflight(env=env))["14"]
        self.assertFalse(check.passed)
        self.assertIn("tokenizer unavailable", check.detail)
        if not tokenizer_available():
            self.skipTest("o200k cache unavailable (TIKTOKEN_CACHE_DIR)")
        check = by_number(self.preflight())["14"]
        self.assertFalse(check.passed)
        self.assertIn("max_transcript_tokens 1 <", check.detail)

    def test_harness_change_and_mode_mismatch(self):
        self.assertEqual(self.create(), 0)
        self.assertFalse(by_number(self.preflight(mode_command="dry-run"))["1"].passed)
        path = self.campaign_dir / "campaign.json"
        spec = json.loads(path.read_text())
        spec["harness_sha256"]["paid_runner.py"] = "0" * 64
        with mock.patch("paid_preflight.open_campaign") as opened:
            real = open_campaign(self.campaign_dir)
            real.spec = {**real.spec, "harness_sha256": spec["harness_sha256"]}
            opened.return_value = real
            check = by_number(self.preflight())["12"]
        self.assertFalse(check.passed)
        self.assertIn("paid_runner.py", check.detail)

    def test_journal_blocking_and_acknowledgement(self):
        self.assertEqual(self.create(), 0)
        sheet = self.fixture.sheet()
        campaign = open_campaign(self.campaign_dir, sheet)
        journal = CallJournal(campaign.directory)
        wire = build_wire(campaign.profile("diagnostic"), messages())
        rid = request_id(campaign.campaign_id, binding(), sha256_hex(wire_bytes(wire)))
        bound = sheet.stage_profile("diagnostic", 1000, True)
        token = campaign.ledger.reserve(rid, str(sheet.max_usd(bound, "p1")))
        clock = itertools.count(1_700_000_000).__next__
        journal.intent({"schema": 1, "request_id": rid, "letter": "F", "stage": "diagnostic", "binding": binding(),
                        "max_usd": str(sheet.max_usd(bound, "p1")), "wire_sha256": sha256_hex(wire_bytes(wire)),
                        "ledger_token": token, "profile": campaign.profile("diagnostic"), "created_at": clock()}, wire)
        journal.sent(rid, clock(), 5, "127.0.0.1")
        check = by_number(self.preflight())["1"]
        self.assertFalse(check.passed)
        self.assertIn("not acknowledged", check.detail)
        self.assertTrue(by_number(self.preflight(acknowledged={rid}))["1"].passed)
        (self.campaign_dir / "run.lock").write_text("{}")
        self.assertIn("run.lock", by_number(self.preflight(acknowledged={rid}))["1"].detail)


if __name__ == "__main__":
    unittest.main()
