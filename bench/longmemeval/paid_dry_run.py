"""``dry-run``: the whole paid path against a loopback fake provider, 0 USD (plan §5).

A temporary ``mode=dry_run`` campaign is created from the same flags as
``create-campaign`` (or an existing one is continued with ``--resume``), the
provider is ``paid_fake_openai.FakeOpenAI`` bound to 127.0.0.1, and the run
goes through preflight (dry-run variant: ``OPENAI_API_KEY`` must be ABSENT),
the control call, the four phases and the report, exercising reservations,
sublimits, the journal and resume exactly as a paid run would. It ends with
the PREDICTED MAXIMUM table for both bound policies plus what was observed:
requests per role and per session, system fingerprints, zero_facts, pins and
every pinned hash. JSON goes to stdout and to ``dry_run_summary.json``.
"""
import hashlib
import json
import os
from pathlib import Path
import secrets
import sys

from paid_call_journal import CallJournal
from paid_campaign import CampaignError, create_campaign, open_campaign
from paid_control import SYSTEM as CONTROL_SYSTEM, control_call, has_settled_control
from paid_fake_openai import FakeOpenAI
from paid_holdout import verify_holdout
from paid_input_tokens import InputTokenCounter, load_counts
from paid_maximum import format_table, predicted_maximum
from paid_openai_wire import Transport
from paid_phases import RunContext, run_phases
from paid_preflight import all_passed, format_checks, run_preflight, tokenizer_or_none
from paid_price_sheet import load_sheet
from paid_prompts import load_prompts
from paid_report import build_report, write_report
from paid_runner_cli import compose_spec, iso_utc, judge_prompt_pair, parse_pairs, parse_profiles

KEY_ENV = "LOOMEM_DRY_RUN_PROVIDER_TOKEN"
STAGES = ("diagnostic", "audit", "selection", "answer", "judge")


def _sha(text):
    return hashlib.sha256(text.encode("utf-8")).hexdigest()


def fake_provider(prompts, profiles, scenarios):
    """Roles by system fingerprint for the five stages and the control probe; the extraction role is
    the only one whose model alias is registered, so an unknown system with that alias resolves to it."""
    roles = {_sha(prompts.stage(stage).system): stage for stage in STAGES}
    roles[_sha(CONTROL_SYSTEM)] = "control"
    models = {"extraction": profiles["extraction"]["model"], "judge": profiles["judge"]["model"]}
    return FakeOpenAI({"default": "ok", **scenarios}, model_by_role=models, role_by_system_sha256=roles,
                      token=secrets.token_hex(16))


def _register_extraction_role(fake):
    def register(proxy):
        if proxy.system_sha256:
            fake.role_by_system.setdefault(proxy.system_sha256, "extraction")
    return register


def _campaign(args, prompts, sheet, fake, clock):
    directory = Path(args.campaign_dir)
    if args.resume:
        campaign = open_campaign(directory, sheet)
        if campaign.mode != "dry_run":
            raise CampaignError(f"--resume needs a dry_run campaign, found mode {campaign.mode!r}")
        return campaign
    spec = compose_spec(args, prompts, mode="dry_run", created_at=iso_utc(clock()), provider_base_url=fake.url,
                        api_key_env=KEY_ENV)
    return create_campaign(directory, spec, sheet)


def _observed(campaign, journal, holdout, fake, counter, prompts, sheet):
    directory = Path(campaign.directory)
    sessions, by_sid = [], {}
    for hid in holdout.histories:
        for session in holdout.histories[hid]["sessions"]:
            by_sid[(hid, session["session_id"])] = session
    from paid_maximum import predicted_chunks, render_transcript
    for path in sorted((directory / "extraction").glob("*.json")) if (directory / "extraction").is_dir() else []:
        extraction = json.loads(path.read_text(encoding="utf-8"))
        for s in extraction.get("sessions") or []:
            text = render_transcript(by_sid[(extraction["history_id"], s["session_id"])])
            sessions.append({"history_id": extraction["history_id"], "session_id": s["session_id"],
                             "observed_requests": len(s["requests"]), "predicted_max_requests": s.get("predicted_max_requests"),
                             "predicted_chunks": predicted_chunks(text, sheet.extraction_chunk_chars()),
                             "bytes": len(text.encode("utf-8")),
                             "o200k_tokens": counter(text) if counter else None, "zero_facts": s["zero_facts"]})
    by_role, by_stage = {}, {}
    for entry in fake.log:
        by_role[entry["role"]] = by_role.get(entry["role"], 0) + 1
    for intent in journal.state.intents.values():
        by_stage[intent["stage"]] = by_stage.get(intent["stage"], 0) + 1
    events = sorted(p.name for p in (directory / "events").glob("*.json")) if (directory / "events").is_dir() else []
    return {"requests_by_role_this_invocation": dict(sorted(by_role.items())),
            "input_token_probes_this_invocation": len(fake.count_log),
            "journal_calls_by_stage": dict(sorted(by_stage.items())),
            "input_token_probes_recorded": len(load_counts(directory)),
            "extraction_requests_per_session": sessions,
            "system_fingerprints": {"extraction": _frozen(directory),
                                    **{s: _sha(prompts.stage(s).system) for s in STAGES}, "control": _sha(CONTROL_SYSTEM)},
            "zero_facts_events": [e for e in events if e.endswith(".zero_facts.json")],
            "context_errors": [e for e in events if e.endswith(".context_error.json")],
            "pins": {alias: pin["model"] for alias, pin in sorted(journal.state.pins.items())}}


def _frozen(directory):
    path = directory / "extraction_system.json"
    return json.loads(path.read_text(encoding="utf-8"))["system_sha256"] if path.is_file() else None


def _hashes(campaign, prompts, sheet):
    spec = campaign.spec
    configs = []
    folder = Path(campaign.directory) / "extraction"
    if folder.is_dir():
        for path in sorted(folder.glob("*.json")):
            value = json.loads(path.read_text(encoding="utf-8")).get("effective_config_sha256")
            if value and value not in configs:
                configs.append(value)
    return {"binary": spec["engine"]["binary_sha256"], "template": spec["engine"]["template_sha256"],
            "effective_config": configs, "manifest": spec["holdout"]["manifest_sha256"],
            "always50": spec["holdout"]["always50_sha256"], "plan_json": prompts.plan_sha256,
            "judge_prompt": prompts.judge_sha256, "cost_sheet": sheet.sha256,
            "stage_sha256": spec["plan_json"]["stage_sha256"], "harness": spec.get("harness_sha256")}


def _write_summary(directory, summary):
    path = Path(directory) / "dry_run_summary.json"
    temporary = path.with_name(path.name + ".tmp")
    temporary.write_text(json.dumps(summary, sort_keys=True, indent=1) + "\n", encoding="utf-8")
    os.replace(temporary, path)
    return path


def cmd_dry_run(args, factories, clock, env, out):
    from paid_runner import STOPS, remember_extraction_system, resolve_factories, stage_map
    if env.get("OPENAI_API_KEY"):
        print("refused: OPENAI_API_KEY is present in the environment; a dry-run must not be able to pay", file=sys.stderr)
        return 2
    sheet = load_sheet(args.cost_sheet, args.cost_sheet_sha256)
    prompts = load_prompts(args.plan_json, args.plan_json_sha256, judge_prompt_pair(args))
    scenarios = parse_pairs(args.scenario, "--scenario")
    holdout = verify_holdout(args.holdout_dir, args.holdout_manifest_sha256, args.always50_sha256)
    if args.fake_engine:
        from paid_fake_engine import fake_engine_factory
        factories = {**(factories or {}), "engine": fake_engine_factory}
    engine_probe = not args.no_engine_probe and not (factories and "engine" in factories)
    with fake_provider(prompts, parse_profiles(args.profile), scenarios) as fake:
        previous = os.environ.get(KEY_ENV)
        os.environ[KEY_ENV] = fake.token
        try:
            return _dry_run(args, factories, clock, env, out, sheet, prompts, holdout, fake, engine_probe,
                            STOPS, remember_extraction_system, resolve_factories, stage_map)
        finally:
            if previous is None:
                os.environ.pop(KEY_ENV, None)
            else:
                os.environ[KEY_ENV] = previous


def _dry_run(args, factories, clock, env, out, sheet, prompts, holdout, fake, engine_probe,
             stops, remember, resolve, stage_map):
    campaign = _campaign(args, prompts, sheet, fake, clock)
    journal = CallJournal(campaign.directory)
    directory = Path(campaign.directory)
    env_view = {**env, KEY_ENV: fake.token}
    checks = run_preflight(directory, mode_command="dry-run", env=env_view, holdout=holdout, sheet=sheet,
                           prompts=prompts, engine_probe=engine_probe, limit_histories=args.limit_histories)
    print(format_checks(checks), file=out)
    if not all_passed(checks):
        return 2
    if _frozen(directory):
        fake.role_by_system.setdefault(_frozen(directory), "extraction")
    engine_factory, proxy_factory = resolve(factories, campaign)
    transport = Transport(fake.url, KEY_ENV, {"127.0.0.1"})
    precount = InputTokenCounter(campaign, directory, fake.url, KEY_ENV, context={}, clock=clock,
                                 stage_by_system_sha256=stage_map(prompts))
    stop = None
    with campaign.run_lock():
        proxy = proxy_factory(campaign=campaign, journal=journal, sheet=sheet, transport=transport,
                              extraction_profile=campaign.profile("extraction"),
                              manifest_sha256=campaign.spec["holdout"]["manifest_sha256"],
                              system_sha256=_frozen(directory), precount=precount, clock=clock)
        register = _register_extraction_role(fake)
        with proxy:
            try:
                if campaign.has_control_profile() and not has_settled_control(journal.state):
                    control_call(campaign, journal, sheet, transport, precount=precount, clock=clock)
                ctx = RunContext(campaign, journal, sheet, prompts, holdout, transport, engine_factory, proxy,
                                 precount=precount, limit_histories=args.limit_histories, clock=clock,
                                 after_session=lambda p: (register(p), remember(directory, p, clock)))
                run_phases(ctx, "all")
            except stops as exc:
                stop = exc
    report = build_report(campaign, journal.state, directory, holdout, clock)
    report_path = write_report(campaign, report)
    maximum = predicted_maximum(sheet, campaign.spec, holdout, limit_histories=args.limit_histories,
                                token_counter=tokenizer_or_none(env))
    summary = {"campaign_id": campaign.campaign_id, "campaign_dir": str(directory), "mode": campaign.mode,
               "resumed": bool(args.resume), "scenarios": {"default": "ok", **parse_pairs(args.scenario, "--scenario")},
               "preflight": [{"name": c.name, "passed": c.passed, "detail": c.detail} for c in checks],
               "predicted_maximum": maximum, "observed": _observed(campaign, journal, holdout, fake, tokenizer_or_none(env), prompts, sheet),
               "hashes": _hashes(campaign, prompts, sheet), "report_path": str(report_path),
               "report": {k: report[k] for k in ("per_arm", "money", "extraction", "input_tokens", "pins", "ledger")},
               "stop": None if stop is None else f"{type(stop).__name__}: {stop}"}
    print(format_table(maximum), file=out)
    print(json.dumps(summary, sort_keys=True, indent=1), file=out)
    _write_summary(directory, summary)
    return 0 if stop is None else 3
