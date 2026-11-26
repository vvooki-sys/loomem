"""Preflight: the PASS/FAIL table every paid start and dry-run must clear (plan §5).

``run_preflight`` returns every check as a ``Check(name, passed, detail)``; it
never raises for a failing check and never spends money. The CLI prints the
table and exits 2 on any FAIL. Checks follow plan §5 (1-13) plus the brief's
additions: #14 compares ``max_transcript_tokens`` with the real o200k count of
every session, #15 requires the control profile in paid mode, and the key check
proves ``OPENAI_API_KEY`` is in the runner env (paid) or absent (dry-run) and
never equals any engine env value. Only for ``--phase review`` (a ``review`` plan is passed), #18
checks the review prompt fingerprint and list hash, the campaign's frozen review state, that every
listed pair has a judged candidate, and that the review maximum fits sublimit R; without a plan the
table is exactly #1-#17.
"""
from dataclasses import dataclass
import os
from pathlib import Path
import platform
import shutil
import sys

from paid_call_journal import CallJournal, reconcile
from paid_campaign import open_campaign, validate_spec
from paid_control import control_reservation
from paid_holdout import EXECUTION_FIELDS, verify_holdout
from paid_maximum import predicted_maximum, render_transcript, selected_histories
from paid_preflight_probes import (artifact_check, effective_config_check, gold_scan, harness_sha256,  # noqa: F401
                                   o200k_counter, sandbox_probe, transcript_tokens_check)
from paid_price_sheet import STAGES, load_sheet
from paid_prompts import load_prompts

PROXY_VARS = ("HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "http_proxy", "https_proxy", "all_proxy")
MIN_FREE_BYTES = 2 * 1024 ** 3
ENGINE_ENV_KEYS = ("PATH", "HOME", "TMPDIR", "RUST_LOG", "LOOMEM_CONFIG", "LOOMEM_AUTH_TOKEN",
                   "LOOMEM_EVALUATION_READINESS", "LOOMEM_BENCH_PROXY_TOKEN",
                   "LOOMEM_EVALUATION_EXTRACTION_TIMEOUT_SECS")


@dataclass
class Check:
    name: str
    passed: bool
    detail: str


def expected_mode(mode_command):
    return "dry_run" if mode_command in ("dry-run", "dry_run") else "paid"


def engine_env_template(env):
    """The variables the engine child receives (``paid_engine.EvaluationEngine.engine_env``: nothing is
    inherited from the runner env; values that are random at start are placeholders here)."""
    return {"PATH": os.defpath, "HOME": "<engine work dir>", "TMPDIR": "<engine work dir>/tmp",
            "RUST_LOG": "error", "LOOMEM_CONFIG": "<engine work dir>/config.toml",
            "LOOMEM_AUTH_TOKEN": "<random at start>", "LOOMEM_EVALUATION_READINESS": "1",
            "LOOMEM_BENCH_PROXY_TOKEN": "<random proxy bearer>",
            "LOOMEM_EVALUATION_EXTRACTION_TIMEOUT_SECS": "<campaign timeout_s>"}


class _Preflight:
    def __init__(self, campaign_dir, mode_command, env, engine_probe, limit_histories, engine_env):
        self.directory = Path(campaign_dir)
        self.mode = expected_mode(mode_command)
        self.env = env
        self.engine_probe = engine_probe
        self.limit_histories = limit_histories
        self.engine_env = engine_env
        self.checks = []
        self.acknowledged = frozenset()
        self.review = None
        self.campaign = self.journal = self.holdout = self.sheet = self.prompts = None

    def add(self, name, passed, detail):
        self.checks.append(Check(name, bool(passed), str(detail)))
        return passed

    def guarded(self, name, fn):
        try:
            passed, detail = fn()
        except Exception as exc:  # a crashing check is a FAIL with its reason, never an abort
            passed, detail = False, f"{type(exc).__name__}: {exc}"
        return self.add(name, passed, detail)

    # 1 ------------------------------------------------------------------
    def check_campaign(self):
        self.campaign = open_campaign(self.directory)
        spec = self.campaign.spec
        if spec["mode"] != self.mode:
            return False, f"campaign mode {spec['mode']!r} does not match command mode {self.mode!r}"
        if (self.directory / "run.lock").exists():
            return False, "run.lock present: another run owns this campaign"
        self.journal = CallJournal(self.directory)
        totals = self.campaign.ledger.totals()
        if totals["breached"]:
            return False, "ledger breached: a settlement exceeded its reservation"
        blocking = self.journal.state.blocking(self.acknowledged)
        if blocking:
            return False, (f"journal has {len(blocking)} unsettled/uncertain request(s) not acknowledged: "
                           f"{blocking[:3]}")
        reconcile(totals, self.journal.state)
        return True, (f"mode {self.mode}; limit {totals['limit_usd']} spent {totals['spent_usd']} "
                      f"reserved {totals['reserved_usd']}; {len(self.journal.state.intents)} intent(s)")

    # 2-4 ----------------------------------------------------------------
    def check_holdout(self):
        pin = self.campaign.spec["holdout"]
        if self.holdout is None:
            self.holdout = verify_holdout(pin["dir"], pin["manifest_sha256"], pin["always50_sha256"])
        elif self.holdout.manifest_sha256 != pin["manifest_sha256"]:
            return False, "supplied holdout does not carry the pinned manifest sha256"
        h = self.holdout
        return True, f"{len(h.histories)} histories, {len(h.questions)} questions, {len(h.always_ids)} always ids pinned"

    def check_prompts(self):
        spec = self.campaign.spec
        judge = spec.get("judge_prompt")
        if self.mode == "paid" and not judge:
            return False, "judge_prompt pin is required in mode=paid"
        if self.prompts is None:
            pair = (judge["path"], judge["sha256"]) if judge else None
            self.prompts = load_prompts(spec["plan_json"]["path"], spec["plan_json"]["sha256"], pair)
        fingerprint = self.prompts.fingerprint()
        stages = {k: v for k, v in fingerprint.items() if k not in ("plan_json", "judge_prompt")}
        if stages != spec["plan_json"]["stage_sha256"]:
            return False, "plan.json stage_sha256 differs from the registry"
        if fingerprint["plan_json"] != spec["plan_json"]["sha256"]:
            return False, "plan.json sha256 differs from the registry"
        if judge and fingerprint["judge_prompt"] != judge["sha256"]:
            return False, "judge prompt sha256 differs from the registry"
        return True, f"5 stages pinned; judge prompt {'pinned' if judge else 'absent (dry-run)'}"

    def check_sheet(self):
        spec = self.campaign.spec
        if self.sheet is None:
            self.sheet = load_sheet(spec["cost_sheet"]["path"], spec["cost_sheet"]["sha256"])
        elif self.sheet.sha256 != spec["cost_sheet"]["sha256"]:
            return False, "supplied sheet does not carry the pinned sha256"
        validate_spec(spec, self.sheet)
        policy = spec["bound_policy"]
        sources = self.holdout.sources_bytes_max() if self.holdout else 0
        for stage in STAGES:
            if stage == "extraction":
                continue
            self.sheet.max_usd(self.sheet.stage_profile(stage, sources, True), policy)
        largest = max(self.holdout.session_bytes(), default=0) if self.holdout else 0
        self.sheet.max_usd(self.sheet.extraction_bound(largest), policy)
        return True, f"profiles match the sheet; every stage priceable under {policy}"

    # 5-7 ----------------------------------------------------------------
    def check_limits(self):
        if self.sheet is None or self.holdout is None:
            return False, "cannot compute the maximum without the sheet and the holdout"
        maximum = predicted_maximum(self.sheet, self.campaign.spec, self.holdout, limit_histories=self.limit_histories)
        policy = maximum["policy_chosen"]
        over = maximum["letters_over_sublimit"][policy]
        if over:
            numbers = {letter: (maximum["by_letter"][letter][policy], maximum["sublimits_usd"][letter]) for letter in over}
            return False, f"predicted maximum exceeds the sublimit for {numbers} (policy {policy})"
        return True, (f"maximum {maximum['total'][policy]} <= sublimits {maximum['sublimits_sum']} "
                      f"<= limit {maximum['limit_usd']} ({policy})")

    def check_policy(self):
        spec = self.campaign.spec
        return True, f"bound_policy {spec['bound_policy']}; p1_framing_accepted={spec['p1_framing_accepted']}"

    def check_sources(self):
        missing = []
        for key in ("reasoning_effort_source", "tokenizer_source"):
            section = self.campaign.spec.get(key)
            if not isinstance(section, dict) or not section.get("url") or not section.get("checked_on"):
                missing.append(key)
        if missing:
            return False, f"missing {missing} (url + checked_on; to be confirmed in the provider docs)"
        return True, "reasoning_effort_source and tokenizer_source recorded with dates"

    # 8 ------------------------------------------------------------------
    def check_key(self):
        key_env = self.campaign.spec["provider"]["api_key_env"]
        engine = self.engine_env if self.engine_env is not None else engine_env_template(self.env)
        if self.mode == "dry_run":
            if self.env.get("OPENAI_API_KEY"):
                return False, "OPENAI_API_KEY is present in the runner env: a dry-run must never be able to pay"
            if not self.env.get(key_env):
                return False, f"dry-run provider token {key_env} missing from the runner env"
            return True, f"OPENAI_API_KEY absent; dry-run token {key_env} present"
        key = self.env.get(key_env)
        if not key:
            return False, f"{key_env} missing from the runner env"
        leaked = sorted(name for name, value in engine.items()
                        if name in ("OPENAI_API_KEY", key_env) or value == key)
        if leaked:
            return False, f"engine env carries the credential via {leaked}"
        extra = sorted(set(engine) - set(ENGINE_ENV_KEYS))
        if extra:
            return False, f"engine env carries unexpected variables {extra}"
        return True, f"{key_env} present in the runner env only; engine env {sorted(engine)}"

    # 9-10 ---------------------------------------------------------------
    def check_sandbox(self):
        if not self.engine_probe:
            return True, "skipped by caller (no engine in this run)"
        passed, detail = sandbox_probe()
        if not passed and self.mode == "dry_run" and "absent" in detail:
            return True, f"skipped: {detail}"
        return passed, detail

    def check_engine(self):
        passed, detail = artifact_check(self.campaign.spec.get("engine"))
        if not passed:
            return False, detail
        if not self.engine_probe:
            return True, detail + "; TOML check skipped by caller"
        toml_ok, toml_detail = effective_config_check(self.campaign.spec["engine"], self.campaign.spec["extraction"])
        return toml_ok, f"{detail}; {toml_detail}"

    # 11-15 --------------------------------------------------------------
    def check_gold(self):
        if len(EXECUTION_FIELDS) != 4:
            return False, "execution_view must expose exactly 4 fields"
        return gold_scan(self.directory, self.journal.state, self.holdout)

    def check_harness(self):
        current = harness_sha256()
        pinned = self.campaign.spec.get("harness_sha256")
        if not isinstance(pinned, dict):
            return False, "registry has no harness_sha256"
        changed = sorted(k for k in set(current) | set(pinned) if current.get(k) != pinned.get(k))
        if changed:
            return False, f"harness changed since campaign creation: {changed} (new campaign required)"
        return True, f"{len(current)} module hashes match the registry"

    def check_environment(self):
        problems = []
        if sys.version_info < (3, 12):
            problems.append(f"python {platform.python_version()} < 3.12")
        proxies = [name for name in PROXY_VARS if self.env.get(name)]
        if proxies:
            problems.append(f"proxy variables set: {proxies}")
        free = shutil.disk_usage(self.directory).free
        if free < MIN_FREE_BYTES:
            problems.append(f"free disk {free // 1024 ** 2} MiB < 2 GiB")
        if problems:
            return False, "; ".join(problems)
        return True, f"python {platform.python_version()}; no proxy variables; free disk {free // 1024 ** 3} GiB"

    def check_transcript_tokens(self):
        histories = selected_histories(self.holdout, self.limit_histories)
        limit = (self.campaign.spec.get("extraction") or {}).get("max_transcript_tokens")
        return transcript_tokens_check(self.env, self.holdout, histories, limit, render_transcript)

    def check_control(self):
        if not self.campaign.has_control_profile():
            if self.mode == "paid":
                return False, "control profile missing (required for the pilot's first paid step)"
            return True, "control profile absent (dry-run)"
        bound, max_usd = control_reservation(self.campaign, self.sheet)
        return True, (f"control (letter K) reserves {max_usd} USD <= 0.2374, sublimit K "
                      f"{self.campaign.sublimits['K']} <= 0.2374 (R={bound.r_tokens}, O={bound.o_tokens})")

    def check_live_engine(self):
        if not self.engine_probe:
            return True, "skipped by caller (no engine in this run)"
        from paid_preflight_engine import format_probe, probe_engine
        result = probe_engine(self.campaign.spec, self.sheet, mode=self.mode)
        return result["passed"], format_probe(result)

    def check_chunking(self):
        from paid_preflight_engine import chunking_matches_sheet
        return chunking_matches_sheet(self.sheet, self.campaign.spec["extraction"])

    # 18 (only with a review plan) -----------------------------------------
    def check_review(self):
        from paid_prompts import REVIEW_PROMPT_SHA256
        from paid_review import check_frozen, review_maximum, reviewable_verdicts
        plan = self.review
        if plan.prompt_sha256 != REVIEW_PROMPT_SHA256:
            return False, "review plan pins a different review prompt fingerprint"
        frozen = check_frozen(self.directory, plan)
        reviewable_verdicts(self.directory, plan)
        per_call, total = review_maximum(self.sheet, self.campaign, self.holdout, len(plan))
        sublimit = self.campaign.sublimits["R"]
        if total > sublimit:
            return False, (f"review maximum {len(plan)} x {per_call} = {total} USD exceeds sublimit R {sublimit} "
                           "(the review needs an explicit R > 0 that covers the whole frozen list)")
        return True, (f"prompt {REVIEW_PROMPT_SHA256[:12]}; list {plan.sha256[:12]} ({len(plan)} pair(s), "
                      f"{'frozen' if frozen else 'freezes on this run'}); every pair judged; maximum "
                      f"{len(plan)} x {per_call} = {total} USD <= sublimit R {sublimit}")

    def run(self):
        if not self.guarded("1 campaign registry/ledger/journal", self.check_campaign):
            return self.checks
        self.guarded("2 holdout pins", self.check_holdout)
        self.guarded("3 plan.json + judge prompt pins", self.check_prompts)
        self.guarded("4 cost sheet pin + profiles", self.check_sheet)
        self.guarded("5 limit and sublimits vs maximum", self.check_limits)
        self.guarded("6 bound policy", self.check_policy)
        self.guarded("7 reasoning_effort/tokenizer sources", self.check_sources)
        self.guarded("8 credential placement", self.check_key)
        self.guarded("9 sandbox EPERM probe", self.check_sandbox)
        self.guarded("10 engine artifacts + effective config", self.check_engine)
        self.guarded("11 gold leak scan", lambda: self.check_gold() if self.holdout else (False, "no holdout"))
        self.guarded("12 harness sha256", self.check_harness)
        self.guarded("13 environment", self.check_environment)
        self.guarded("14 max_transcript_tokens vs o200k", lambda: self.check_transcript_tokens() if self.holdout
                     else (False, "no holdout"))
        self.guarded("15 control profile", lambda: self.check_control() if self.sheet else (False, "no sheet"))
        self.guarded("16 live engine probe", lambda: self.check_live_engine() if self.sheet else (False, "no sheet"))
        self.guarded("17 sheet chunking vs engine", lambda: self.check_chunking() if self.sheet else (False, "no sheet"))
        if self.review is not None:
            self.guarded("18 review prompt + list pins", lambda: self.check_review() if self.sheet and self.holdout
                         else (False, "no sheet or holdout"))
        return self.checks


def run_preflight(campaign_dir, *, mode_command, env=None, holdout=None, sheet=None, prompts=None,
                  engine_probe=True, limit_histories=None, engine_env=None, acknowledged=frozenset(), review=None):
    flight = _Preflight(campaign_dir, mode_command, os.environ if env is None else env, engine_probe,
                        limit_histories, engine_env)
    flight.holdout, flight.sheet, flight.prompts = holdout, sheet, prompts
    flight.acknowledged = frozenset(acknowledged)
    flight.review = review
    return flight.run()


def all_passed(checks):
    return all(check.passed for check in checks)


def format_checks(checks):
    return "\n".join(f"{'PASS' if c.passed else 'FAIL'}  {c.name:<40} {c.detail}" for c in checks)


def tokenizer_or_none(env=None):
    """o200k counter for reporting; None when the cache is unavailable (never downloads)."""
    try:
        return o200k_counter(os.environ if env is None else env)
    except RuntimeError:
        return None
