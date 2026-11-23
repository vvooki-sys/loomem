"""argparse surface of ``paid_runner`` and the composition of ``campaign.json`` (plan §9).

Every pin (sheet, plan, judge prompt, holdout manifest, always-50) is a value
the owner passes explicitly; nothing is read from a live server or guessed.
Engine artifact hashes are computed from the files and, when the owner also
passes them, must agree. The extraction settings are the frozen owner
decision (16384 / 4000 / split / 20 / floor 0) plus ``--max-transcript-tokens``
(default 20000, raised when preflight #14 asks for it).
"""
import argparse
from datetime import datetime, timezone
import hashlib
from pathlib import Path
from urllib.parse import urlsplit

from paid_campaign import CampaignError
from paid_preflight import harness_sha256
from paid_preflight_probes import sha256_text
from smoke_local_e1 import PROFILE

PHASES = ("extraction", "retrieval", "arms", "judge", "all")
EXTRACTION_DEFAULTS = {"max_completion_tokens": 16384, "input_chunk_tokens": 4000, "split_on_truncation": True,
                       "max_facts_per_chunk": 20, "completion_tokens_floor": 0}
DEFAULT_MAX_TRANSCRIPT_TOKENS = 20000
INPUT_TOKENS_DOCS = "https://developers.openai.com/api/docs/api-reference/responses/input-tokens"
INPUT_TOKENS_BILLING = "not_stated_in_docs_2026-11-22"


def add_campaign_flags(parser, dry_run=False):
    add = parser.add_argument
    add("--campaign-dir", required=True)
    if not dry_run:
        add("--mode", choices=("paid", "dry_run"), default="paid")
    add("--limit-usd", required=True, help="explicit campaign limit in USD (no default)")
    add("--sublimit", action="append", default=[], metavar="LETTER=USD", help="E/F/S/A/R, each required")
    add("--bound-policy", choices=("p1", "p2"), default="p1")
    add("--accept-p1-framing", action="store_true")
    add("--cost-sheet", required=True)
    add("--cost-sheet-sha256", required=True)
    add("--plan-json", required=True)
    add("--plan-json-sha256", required=True)
    add("--judge-prompt")
    add("--judge-prompt-sha256")
    add("--holdout-dir", required=True)
    add("--holdout-manifest-sha256", required=True)
    add("--always50-sha256", required=True)
    add("--binary", required=True)
    add("--binary-sha256")
    add("--template", required=True)
    add("--template-sha256")
    add("--model-dir", required=True)
    add("--model-onnx-sha256")
    add("--tokenizer-sha256")
    add("--profile", action="append", default=[], metavar="STAGE=MODEL:EFFORT:MAX_COMPLETION_TOKENS")
    add("--reasoning-effort-source", required=True, metavar="URL@YYYY-MM-DD")
    add("--tokenizer-source", required=True, metavar="URL@YYYY-MM-DD")
    add("--provider-base-url", default="https://api.openai.com/v1")
    add("--api-key-env", default="OPENAI_API_KEY")
    add("--top-k", type=int, default=60)
    add("--timeout-s", type=float, default=600.0)
    add("--max-transcript-tokens", "--extraction-max-transcript-tokens", dest="max_transcript_tokens",
        type=int, default=DEFAULT_MAX_TRANSCRIPT_TOKENS)


def build_parser():
    parser = argparse.ArgumentParser(prog="paid_runner.py",
                                     description="Paid LongMemEval holdout runner (plan B2): create-campaign, "
                                                 "preflight, dry-run, run, report. No call leaves without a "
                                                 "durable reservation.")
    sub = parser.add_subparsers(dest="command", required=True)
    add_campaign_flags(sub.add_parser("create-campaign", help="write campaign.json + budget.json (create-only)"))
    pre = sub.add_parser("preflight", help="0 USD PASS/FAIL table; exit 2 on any FAIL")
    pre.add_argument("--campaign-dir", required=True)
    pre.add_argument("--no-engine-probe", action="store_true", help="skip sandbox/TOML probes (tests only)")
    dry = sub.add_parser("dry-run", help="temporary mode=dry_run campaign against a loopback fake provider")
    add_campaign_flags(dry, dry_run=True)
    dry.add_argument("--limit-histories", type=int)
    dry.add_argument("--scenario", action="append", default=[], metavar="ROLE=SCENARIO")
    dry.add_argument("--resume", action="store_true", help="continue an existing dry_run campaign dir")
    dry.add_argument("--no-engine-probe", action="store_true")
    dry.add_argument("--fake-engine", action="store_true",
                     help="paid_fake_engine instead of the sandboxed binary (no process; exercises proxy/journal only)")
    run = sub.add_parser("run", help="paid phases (mode=paid only)")
    run.add_argument("--campaign-dir", required=True)
    run.add_argument("--phase", choices=PHASES, default="all")
    run.add_argument("--acknowledge-uncertain", action="append", default=[], metavar="RID")
    rep = sub.add_parser("report", help="write reports/report-<n>.json")
    rep.add_argument("--campaign-dir", required=True)
    return parser


# -- flag parsing ----------------------------------------------------------------
def parse_pairs(values, label):
    out = {}
    for value in values:
        if "=" not in value:
            raise CampaignError(f"{label} must be KEY=VALUE, got {value!r}")
        key, _, rest = value.partition("=")
        out[key.strip()] = rest.strip()
    return out


def parse_profiles(values):
    profiles = {}
    for stage, rest in parse_pairs(values, "--profile").items():
        parts = rest.split(":")
        if len(parts) != 3 or not parts[2].isdigit():
            raise CampaignError(f"--profile {stage}: expected MODEL:EFFORT:MAX_COMPLETION_TOKENS")
        profiles[stage] = {"model": parts[0], "reasoning_effort": parts[1], "max_completion_tokens": int(parts[2])}
    return profiles


def parse_source(value, label):
    url, sep, date = value.rpartition("@")
    if not sep or not url or not date:
        raise CampaignError(f"{label} must be URL@YYYY-MM-DD")
    return {"url": url, "checked_on": date}


def _sha256_file(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def _pinned(path, given, label):
    path = Path(path).resolve()
    if not path.is_file():
        raise CampaignError(f"{label} not found: {path}")
    actual = _sha256_file(path)
    if given is not None and given != actual:
        raise CampaignError(f"{label} sha256 mismatch: given {given}, file {actual}")
    return str(path), actual


def engine_section(args):
    binary, binary_sha = _pinned(args.binary, args.binary_sha256, "--binary")
    template, template_sha = _pinned(args.template, args.template_sha256, "--template")
    model_dir = Path(args.model_dir).resolve()
    _, onnx_sha = _pinned(model_dir / "model.onnx", args.model_onnx_sha256, "model.onnx")
    _, tok_sha = _pinned(model_dir / "tokenizer.json", args.tokenizer_sha256, "tokenizer.json")
    return {"binary": binary, "binary_sha256": binary_sha, "template": template, "template_sha256": template_sha,
            "model_dir": str(model_dir), "model_onnx_sha256": onnx_sha, "tokenizer_sha256": tok_sha,
            "sandbox_profile_sha256": sha256_text(PROFILE), "effective_config_sha256": None}


def judge_prompt_pair(args):
    if bool(args.judge_prompt) != bool(args.judge_prompt_sha256):
        raise CampaignError("--judge-prompt and --judge-prompt-sha256 go together")
    if not args.judge_prompt:
        return None
    return str(Path(args.judge_prompt).resolve()), args.judge_prompt_sha256


def compose_spec(args, prompts, *, mode, created_at, provider_base_url=None, api_key_env=None):
    """The campaign.json spec (without campaign_id/nonce) from parsed flags and loaded prompts."""
    base_url = provider_base_url or args.provider_base_url
    host = urlsplit(base_url).hostname
    if not host:
        raise CampaignError(f"provider base url has no host: {base_url!r}")
    fingerprint = prompts.fingerprint()
    judge = judge_prompt_pair(args)
    spec = {
        "schema": 1, "mode": mode, "created_at": created_at,
        "limit_usd": args.limit_usd, "sublimits_usd": parse_pairs(args.sublimit, "--sublimit"),
        "bound_policy": args.bound_policy, "p1_framing_accepted": bool(args.accept_p1_framing),
        "cost_sheet": {"path": str(Path(args.cost_sheet).resolve()), "sha256": args.cost_sheet_sha256},
        "plan_json": {"path": str(Path(args.plan_json).resolve()), "sha256": args.plan_json_sha256,
                      "stage_sha256": {k: v for k, v in fingerprint.items() if k not in ("plan_json", "judge_prompt")}},
        "holdout": {"dir": str(Path(args.holdout_dir).resolve()), "manifest_sha256": args.holdout_manifest_sha256,
                    "always50_sha256": args.always50_sha256},
        "profiles": parse_profiles(args.profile),
        "provider": {"base_url": base_url, "api_key_env": api_key_env or args.api_key_env, "allowed_host": host},
        "timeout_s": args.timeout_s, "judge_sees_sources": True,
        "engine": engine_section(args),
        "extraction": {**EXTRACTION_DEFAULTS, "max_transcript_tokens": args.max_transcript_tokens},
        "search": {"top_k": args.top_k, "reference_time_from_question_date": True},
        "input_tokens": {"enabled": True, "billing": INPUT_TOKENS_BILLING, "docs_url": INPUT_TOKENS_DOCS},
        "reasoning_effort_source": parse_source(args.reasoning_effort_source, "--reasoning-effort-source"),
        "tokenizer_source": parse_source(args.tokenizer_source, "--tokenizer-source"),
        "harness_sha256": harness_sha256(),
    }
    if judge is not None:
        spec["judge_prompt"] = {"path": judge[0], "sha256": judge[1]}
    return spec


def iso_utc(timestamp):
    return datetime.fromtimestamp(timestamp, tz=timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
