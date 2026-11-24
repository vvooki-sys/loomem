#!/usr/bin/env python3
"""Paid LongMemEval holdout runner (plan B2): create-campaign, preflight, dry-run, run, report.

``run`` is the only command that can spend money and it refuses unless the
campaign is ``mode=paid``, every preflight check passes, ``run.lock`` is free
and the journal has no unsettled/uncertain request that is not explicitly
acknowledged. It then makes the pilot's control call once (skipped when a
settled control request exists) and executes the requested phases through
``paid_phases`` (``--phase control`` stops after that call); every provider
request goes through ``paid_completion`` with the exact pre-send input count as
``precount``. ``dry-run`` lives in ``paid_dry_run`` and talks only to a loopback
fake. Exit codes: 0 ok, 2 refused/FAIL, 3 stopped mid-run (journal and report
say where). An extraction the product capped (facts above ``max_facts_per_chunk``
dropped, engine status ``partial``) is the state ``extraction_capped``, not a
stop: the session stays in the denominator and the report counts it. A provider rejection
(``rejected_4xx``, e.g. HTTP 429 ``insufficient_quota``) stops the run and replays as a stop;
``--retry-rejected RID`` (each RID settled ``rejected_4xx``) sends that same frozen request again
as ``attempt + 1``, which every later resume then replays.
"""
from dataclasses import dataclass
import os
from pathlib import Path
import sys
import time

from paid_call_journal import CallJournal, JournalError
from paid_campaign import CampaignError, create_campaign, open_campaign
from paid_control import control_call, has_settled_control
from paid_holdout import HoldoutError, verify_holdout
from paid_input_tokens import InputTokenCounter
from paid_openai_wire import Transport, WireError
from paid_phases import RunContext, run_phases
from paid_preflight import all_passed, format_checks, run_preflight
from paid_price_sheet import SheetError, load_sheet
from paid_prompts import PromptsError, load_prompts
from paid_report import build_report, write_report
from paid_runner_cli import build_parser, compose_spec, iso_utc, judge_prompt_pair

REFUSALS = (CampaignError, SheetError, PromptsError, HoldoutError, JournalError, WireError, OSError, ValueError)
# Every fail-closed stop (RunStopped family, BudgetError, CampaignError, JournalError, EngineError,
# ProxyHalted are RuntimeErrors; WireError, SheetError, ContextError are ValueErrors) ends the run
# with a report and exit 3; genuine programming errors (TypeError, KeyError) still surface as tracebacks.
STOPS = (RuntimeError, ValueError, OSError)


@dataclass
class Loaded:
    campaign: object
    sheet: object
    prompts: object
    holdout: object
    journal: object


def load_campaign(directory):
    """Open a campaign with its pinned sheet, prompts and holdout (all hash-checked)."""
    first = open_campaign(directory)
    spec = first.spec
    sheet = load_sheet(spec["cost_sheet"]["path"], spec["cost_sheet"]["sha256"])
    campaign = open_campaign(directory, sheet)
    judge = spec.get("judge_prompt")
    prompts = load_prompts(spec["plan_json"]["path"], spec["plan_json"]["sha256"],
                           (judge["path"], judge["sha256"]) if judge else None)
    holdout = verify_holdout(spec["holdout"]["dir"], spec["holdout"]["manifest_sha256"], spec["holdout"]["always50_sha256"])
    return Loaded(campaign, sheet, prompts, holdout, CallJournal(directory))


def stage_map(prompts):
    import hashlib
    from paid_control import SYSTEM
    out = {hashlib.sha256(prompts.stage(n).system.encode("utf-8")).hexdigest(): n
           for n in ("diagnostic", "audit", "selection", "answer", "judge")}
    out[hashlib.sha256(SYSTEM.encode("utf-8")).hexdigest()] = "control"
    return out


# -- factories (the real engine and proxy; tests inject fakes) ---------------
def default_engine_factory(campaign):
    def factory(proxy_url, proxy_token):
        from paid_engine import EvaluationEngine
        engine = campaign.spec["engine"]
        artifacts = {k: engine[k] for k in ("binary", "binary_sha256", "template", "template_sha256",
                                            "model_dir", "model_onnx_sha256", "tokenizer_sha256")}
        return EvaluationEngine(artifacts, proxy_url=proxy_url, proxy_token=proxy_token,
                                extraction=campaign.spec["extraction"], top_k=campaign.spec["search"]["top_k"],
                                extraction_timeout_s=int(campaign.spec["timeout_s"]))
    return factory


def default_proxy_factory(**kwargs):
    from paid_provider_proxy import ProviderProxy
    return ProviderProxy(**kwargs)


def resolve_factories(factories, campaign):
    factories = dict(factories or {})
    engine = factories.get("engine")
    return ((engine(campaign) if engine is not None else default_engine_factory(campaign)),
            factories.get("proxy", default_proxy_factory))


# -- commands ------------------------------------------------------------------
def cmd_create(args, clock, out):
    sheet = load_sheet(args.cost_sheet, args.cost_sheet_sha256)
    prompts = load_prompts(args.plan_json, args.plan_json_sha256, judge_prompt_pair(args))
    spec = compose_spec(args, prompts, mode=args.mode, created_at=iso_utc(clock()))
    campaign = create_campaign(args.campaign_dir, spec, sheet)
    print(f"created campaign {campaign.campaign_id} in {campaign.directory} (mode={campaign.mode}, "
          f"limit={campaign.limit} USD)", file=out)
    return 0


def cmd_preflight(args, env, out, acknowledged=frozenset()):
    loaded = None
    try:
        loaded = load_campaign(args.campaign_dir)
    except REFUSALS as exc:
        print(f"note: {type(exc).__name__}: {exc}", file=out)
    checks = run_preflight(args.campaign_dir, mode_command="preflight", env=env,
                           holdout=loaded.holdout if loaded else None, sheet=loaded.sheet if loaded else None,
                           prompts=loaded.prompts if loaded else None, engine_probe=not args.no_engine_probe,
                           acknowledged=acknowledged)
    print(format_checks(checks), file=out)
    return 0 if all_passed(checks) else 2


def cmd_report(args, clock, out):
    loaded = load_campaign(args.campaign_dir)
    report = build_report(loaded.campaign, loaded.journal.state, loaded.campaign.directory, loaded.holdout, clock)
    path = write_report(loaded.campaign, report)
    print(str(path), file=out)
    return 0


def _acknowledged(args, journal):
    acknowledged = frozenset(args.acknowledge_uncertain)
    unknown = sorted(r for r in acknowledged if r not in journal.state.intents)
    if unknown:
        raise CampaignError(f"--acknowledge-uncertain names unknown request ids {unknown}")
    return acknowledged


def retry_rejected_ids(requested, journal, acknowledged=frozenset()):
    """``--retry-rejected``: every id must be in the journal, settled ``rejected_4xx`` and not also
    acknowledged (an acknowledged binding becomes a permanent technical_error, so the retry could never
    happen); refused before any preflight probe or reservation otherwise."""
    retry = frozenset(requested or ())
    both = sorted(retry & frozenset(acknowledged))
    if both:
        raise CampaignError(f"--retry-rejected and --acknowledge-uncertain name the same request ids {both}")
    unknown = sorted(r for r in retry if r not in journal.state.intents)
    if unknown:
        raise CampaignError(f"--retry-rejected names unknown request ids {unknown}")
    other = sorted(f"{r} ({journal.state.settled.get(r, {}).get('outcome', 'unsettled')})" for r in retry
                   if r not in unknown and journal.state.settled.get(r, {}).get("outcome") != "rejected_4xx")
    if other:
        raise CampaignError(f"--retry-rejected names request ids not settled as rejected_4xx: {other}")
    return retry


def retry_summary(campaign_id, state, retry):
    """One line: which --retry-rejected ids now have a next attempt in the journal and which were not
    reached by this invocation (e.g. their stage is outside --phase, or the run stopped earlier)."""
    from paid_call_journal import request_id
    used, unused = [], []
    for rid in sorted(retry):
        intent = state.intents[rid]
        following = {**intent["binding"], "attempt": intent["binding"].get("attempt", 1) + 1}
        sent_again = request_id(campaign_id, following, intent["wire_sha256"]) in state.intents
        (used if sent_again else unused).append(rid[:12])
    return f"retry-rejected: sent again {used}; not reached {unused}"


def cmd_run(args, factories, clock, env, out):
    loaded = load_campaign(args.campaign_dir)
    campaign, journal = loaded.campaign, loaded.journal
    campaign.require_paid_mode()
    acknowledged = _acknowledged(args, journal)
    retry = retry_rejected_ids(args.retry_rejected, journal, acknowledged)
    checks = run_preflight(args.campaign_dir, mode_command="run", env=env, holdout=loaded.holdout,
                           sheet=loaded.sheet, prompts=loaded.prompts, acknowledged=acknowledged)
    print(format_checks(checks), file=out)
    if not all_passed(checks):
        return 2
    blocking = journal.state.blocking(acknowledged)
    if blocking:
        print(f"refused: {len(blocking)} unsettled/uncertain request(s) need --acknowledge-uncertain: "
              f"{blocking[:5]}", file=out)
        return 2
    engine_factory, proxy_factory = resolve_factories(factories, campaign)
    provider = campaign.spec["provider"]
    transport = Transport.from_campaign(campaign)
    precount = InputTokenCounter(campaign, campaign.directory, provider["base_url"], provider["api_key_env"],
                                 context={}, clock=clock, stage_by_system_sha256=stage_map(loaded.prompts))
    stop = None
    with campaign.run_lock():
        proxy = proxy_factory(campaign=campaign, journal=journal, sheet=loaded.sheet, transport=transport,
                              extraction_profile=campaign.profile("extraction"),
                              manifest_sha256=campaign.spec["holdout"]["manifest_sha256"],
                              # The pin from the dry-run binds the proxy from the FIRST paid request: an engine
                              # sending any other extraction prompt halts before anything is reserved.
                              system_sha256=(frozen_extraction_system(campaign.directory)
                                             or campaign.spec.get("extraction_system_sha256")),
                              precount=precount, acknowledged=acknowledged, clock=clock, retry_rejected=retry)
        with proxy:
            try:
                if not has_settled_control(journal.state):
                    done = control_call(campaign, journal, loaded.sheet, transport, precount=precount,
                                        acknowledged=acknowledged, clock=clock, retry_rejected=retry)
                    print(f"control call {done.request_id[:12]}: {done.outcome}, model {done.model}", file=out)
                ctx = RunContext(campaign, journal, loaded.sheet, loaded.prompts, loaded.holdout, transport,
                                 engine_factory, proxy, precount=precount, acknowledged=acknowledged, clock=clock,
                                 after_session=lambda p: remember_extraction_system(campaign.directory, p, clock),
                                 retry_rejected=retry)
                run_phases(ctx, args.phase)
            except STOPS as exc:
                stop = exc
    report = build_report(campaign, journal.state, campaign.directory, loaded.holdout, clock)
    print(str(write_report(campaign, report)), file=out)
    if retry:
        print(retry_summary(campaign.campaign_id, journal.state, retry), file=out)
    if stop is not None:
        print(f"STOP: {type(stop).__name__}: {stop}", file=sys.stderr)
        return 3
    return 0


# -- the frozen extraction system fingerprint (create-only) ---------------------
def frozen_extraction_system(directory):
    path = Path(directory) / "extraction_system.json"
    if not path.is_file():
        return None
    import json
    return json.loads(path.read_text(encoding="utf-8"))["system_sha256"]


def remember_extraction_system(directory, proxy, clock):
    from paid_call_journal import write_create_only
    path = Path(directory) / "extraction_system.json"
    if proxy.system_sha256 and not path.exists():
        first = next((r for r in proxy.records if r.get("system_sha256") == proxy.system_sha256), {})
        write_create_only(path, {"system_sha256": proxy.system_sha256,
                                 "first_seen": {"history_id": first.get("history_id"),
                                                "session_id": first.get("session_id"), "at": clock()}})


def main(argv=None, *, factories=None, clock=time.time, stdout=None, env=None):
    out = stdout or sys.stdout
    env = os.environ if env is None else env
    try:
        args = build_parser().parse_args(argv)
    except SystemExit as exc:
        return int(exc.code or 0)
    try:
        if args.command == "create-campaign":
            return cmd_create(args, clock, out)
        if args.command == "preflight":
            return cmd_preflight(args, env, out)
        if args.command == "report":
            return cmd_report(args, clock, out)
        if args.command == "run":
            return cmd_run(args, factories, clock, env, out)
        from paid_dry_run import cmd_dry_run
        return cmd_dry_run(args, factories, clock, env, out)
    except REFUSALS as exc:
        print(f"refused: {type(exc).__name__}: {exc}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
