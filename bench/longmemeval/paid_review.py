"""The ``review`` phase: one blind semantic-audit call per frozen (question, arm) pair (plan §8C/§9).

Run only as ``run --phase review`` (never part of ``--phase all``) with a review list pinned by
``--review-list-sha256`` and the frozen review prompt pinned by ``--review-prompt-sha256``. On its
first run the list's exact bytes are frozen into ``<campaign>/review/list.json`` with
``review/frozen.json`` (create-only); a campaign has one list and one prompt for good.

Each pair is one ``paid_completion`` (letter ``R``, stage ``review``): the sheet's ``review`` bound
(P1 ``R_i``) and the campaign's ``review`` profile ``O``, the exact pre-send input count (stop above
``R_i`` and, as a hard limit, above the model's standard tier), reservation before HTTP, settlement
after, the model pinned per alias (a change is ``ModelDrift``, a stop). Replays cost nothing: a pair
with ``reviews/<qid>.<arm>.json`` is skipped and an identical settled request replays from the
journal. The number of reviewed pairs can never exceed the frozen list (``ReviewCapExceeded``).

Blindness: the model-visible payload is sources, question, reference, candidate and the judge's
verdict — no arm name, arm key, question/history id, file name or path. The binding, journal,
count records and the artifact name carry the arm for bookkeeping; none of them is sent.

The phase never changes a verdict or a score: it writes review artifacts. Corrections are applied
by a separate, frozen policy document on the validation side.
"""
from dataclasses import asdict, dataclass
import hashlib
import json
import os
from pathlib import Path

from paid_arms import ARMS, stage_binding
from paid_call_journal import JournalError, write_create_only
from paid_context import ContextError, byte_caps_ok
from paid_judge import SOURCE_SCOPE, gold_reference, read_verdict, source_conversations
from paid_openai_wire import InputTokensBreach, RunStopped, paid_completion
from paid_prompts import (REVIEW_PROMPT_SHA256, REVIEW_RESPONSE_SCHEMA, REVIEW_STAGE, review_messages,
                          review_response_format)

LETTER = "R"
REVIEWABLE_SCORES = ("correct", "incorrect", "invalid_judge")
REVIEW_KEYS = frozenset(REVIEW_RESPONSE_SCHEMA["required"])
EVIDENCE_KEYS = frozenset({"kind", "source_id", "quote"})
RATIONALE_MAX_CHARS = 1200
QUOTE_MAX_CHARS = 2000
VERDICT_FIELDS = ("score", "status", "correct", "answer_span", "reason")


class ReviewListError(ValueError):
    """The review list is not the pinned one or is malformed; refused before any request."""


class ReviewCapExceeded(RunStopped):
    """A new review would exceed the frozen list (or the journal holds a review outside it)."""


def _sha256(data):
    return hashlib.sha256(data).hexdigest()


CORRECTIONS = tuple(REVIEW_RESPONSE_SCHEMA["properties"]["proposed_correction"]["enum"])
CRITICAL = tuple(REVIEW_RESPONSE_SCHEMA["properties"]["critical_error"]["enum"])
EVIDENCE_KINDS = tuple(REVIEW_RESPONSE_SCHEMA["properties"]["evidence"]["properties"]["kind"]["enum"])


# -- the frozen list -----------------------------------------------------------
@dataclass(frozen=True)
class ReviewPlan:
    sha256: str
    pairs: tuple          # ((question_id, arm), ...) in list order
    raw: bytes
    prompt_sha256: str

    def __len__(self):
        return len(self.pairs)


def _unique_pairs_hook(pairs):
    out = {}
    for key, value in pairs:
        if key in out:
            raise ReviewListError(f"duplicate key {key!r} in the review list")
        out[key] = value
    return out


def parse_review_list(raw, holdout):
    """``{"schema": 1, "pairs": [{"question_id", "arm"}, ...], ...metadata}`` -> pairs tuple."""
    try:
        data = json.loads(raw.decode("utf-8"), object_pairs_hook=_unique_pairs_hook)
    except (UnicodeDecodeError, ValueError) as exc:
        raise ReviewListError(f"review list is not JSON: {exc}") from exc
    if not isinstance(data, dict) or data.get("schema") != 1 or not isinstance(data.get("pairs"), list):
        raise ReviewListError("review list must be an object with schema 1 and a pairs list")
    pairs, seen = [], set()
    for entry in data["pairs"]:
        if not isinstance(entry, dict) or set(entry) != {"question_id", "arm"}:
            raise ReviewListError("every pair must be exactly {question_id, arm}")
        qid, arm = entry["question_id"], entry["arm"]
        if arm not in ARMS:
            raise ReviewListError(f"pair {qid!r}: unknown arm {arm!r}")
        if qid not in holdout.questions:
            raise ReviewListError(f"pair {qid!r}: not a holdout question")
        if arm == "always_originals" and qid not in holdout.always_ids:
            raise ReviewListError(f"pair {qid!r}: always_originals only runs on the always list")
        if (qid, arm) in seen:
            raise ReviewListError(f"pair ({qid!r}, {arm!r}) listed twice")
        seen.add((qid, arm))
        pairs.append((qid, arm))
    if not pairs:
        raise ReviewListError("review list has no pairs")
    return tuple(pairs)


def load_review_plan(path, expected_sha256, prompt_sha256, holdout):
    """Hash-pinned list + the operator's pin of the frozen review prompt (both must match)."""
    if not isinstance(expected_sha256, str) or len(expected_sha256) != 64:
        raise ReviewListError("--review-list-sha256 must be a 64-hex SHA-256")
    if prompt_sha256 != REVIEW_PROMPT_SHA256:
        raise ReviewListError(f"--review-prompt-sha256 {str(prompt_sha256)[:12]} is not this harness's frozen "
                              f"review prompt {REVIEW_PROMPT_SHA256[:12]}")
    path = Path(path)
    if not path.is_file():
        raise ReviewListError(f"review list not found: {path}")
    raw = path.read_bytes()
    actual = _sha256(raw)
    if actual != expected_sha256:
        raise ReviewListError(f"review list SHA-256 mismatch: expected {expected_sha256}, got {actual}")
    return ReviewPlan(actual, parse_review_list(raw, holdout), raw, prompt_sha256)


def plan_from_args(args, holdout):
    """The ReviewPlan for ``--phase review`` (all three pins required); None for any other phase,
    where a review flag is refused instead of silently ignored."""
    given = [args.review_list, args.review_list_sha256, args.review_prompt_sha256]
    if args.phase != "review":
        if any(value is not None for value in given):
            raise ReviewListError("--review-list/--review-list-sha256/--review-prompt-sha256 need --phase review")
        return None
    if any(value is None for value in given):
        raise ReviewListError("--phase review needs --review-list, --review-list-sha256 and --review-prompt-sha256")
    return load_review_plan(args.review_list, args.review_list_sha256, args.review_prompt_sha256, holdout)


def frozen_path(directory):
    return Path(directory) / "review" / "frozen.json"


def read_frozen(directory):
    path = frozen_path(directory)
    if not path.is_file():
        return None
    frozen = json.loads(path.read_text(encoding="utf-8"))
    copy = Path(directory) / "review" / "list.json"
    if not copy.is_file() or _sha256(copy.read_bytes()) != frozen.get("review_list_sha256"):
        raise ReviewListError("review/list.json is missing or differs from review/frozen.json")
    return frozen


def check_frozen(directory, plan):
    """None when nothing is frozen yet; otherwise the frozen record, which must match ``plan``."""
    frozen = read_frozen(directory)
    if frozen is None:
        return None
    if frozen["review_list_sha256"] != plan.sha256:
        raise ReviewListError(f"this campaign froze review list {frozen['review_list_sha256'][:12]}; "
                              f"{plan.sha256[:12]} is refused (one list per campaign)")
    if frozen["review_prompt_sha256"] != plan.prompt_sha256:
        raise ReviewListError("this campaign froze a different review prompt fingerprint")
    return frozen


def _write_bytes_create_only(path, data):
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(f".{path.name}.tmp.{os.getpid()}")
    descriptor = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    try:
        with os.fdopen(descriptor, "wb") as handle:
            handle.write(data)
            handle.flush()
            os.fsync(handle.fileno())
        try:
            os.link(temporary, path)
        except FileExistsError as exc:
            raise JournalError(f"record already exists: {path.name}") from exc
    finally:
        if temporary.exists():
            os.unlink(temporary)


def freeze(directory, plan, clock):
    """First review run: the list's exact bytes and the pins become create-only campaign records."""
    if check_frozen(directory, plan) is not None:
        return
    _write_bytes_create_only(Path(directory) / "review" / "list.json", plan.raw)
    write_create_only(frozen_path(directory), {"schema": 1, "review_list_sha256": plan.sha256,
                                               "review_prompt_sha256": plan.prompt_sha256,
                                               "pairs": len(plan), "frozen_at": clock()})


# -- cap ---------------------------------------------------------------------------
def reviewed_pairs(state):
    """(question_id, arm) of every review-stage intent in the journal."""
    return {(i["binding"].get("question_id"), i["binding"].get("arm"))
            for i in state.intents.values() if i.get("stage") == REVIEW_STAGE}


def check_cap(state, plan, qid, arm):
    """Before a new review request: the journal may only hold listed pairs, and at most len(list)."""
    pairs = reviewed_pairs(state)
    outside = sorted(pairs - set(plan.pairs))
    if outside:
        raise ReviewCapExceeded(f"journal holds review requests for pairs outside the frozen list: {outside[:3]}")
    if (qid, arm) not in pairs and len(pairs) + 1 > len(plan):
        raise ReviewCapExceeded(f"review of ({qid}, {arm}) would make {len(pairs) + 1} reviews, above the "
                                f"frozen list's {len(plan)}: stop, a new approval is needed")


# -- the model-visible payload ------------------------------------------------------
def judge_verdict_view(record):
    """The judge's score with its verdict fields; no request id, arm or outcome."""
    verdict = record.get("verdict") or {}
    view = {"score": record["score"]}
    for key in VERDICT_FIELDS[1:]:
        view[key] = verdict.get(key)
    return view


def review_parts(holdout, question_id, verdict_record, caps):
    question = holdout.execution_view(question_id)
    reference = gold_reference(holdout, question_id, caps["QUESTION_CAP"])
    sources = source_conversations(holdout.histories[question["history_id"]])
    parts = {"source_scope": SOURCE_SCOPE, "source_conversations": sources, "question": question["question"],
             "reference": reference, "candidate": verdict_record["candidate"],
             "judge_verdict": judge_verdict_view(verdict_record)}
    byte_caps_ok(REVIEW_STAGE, parts, caps)
    return parts


# -- parsing --------------------------------------------------------------------------
@dataclass
class Review:
    valid: bool
    agrees_with_judge: bool | None
    proposed_correction: str | None
    critical_error: str | None
    evidence: dict | None
    rationale: str | None
    error: str | None

    def to_dict(self):
        return asdict(self)


def _invalid(error):
    return Review(False, None, None, None, None, None, error)


def _strings(value):
    if isinstance(value, str):
        yield value
    elif isinstance(value, list):
        for item in value:
            yield from _strings(item)
    elif isinstance(value, dict):
        for item in value.values():
            yield from _strings(item)


def _evidence_problem(evidence, reference, sources):
    if not isinstance(evidence, dict) or set(evidence) != EVIDENCE_KEYS:
        return "evidence must be exactly {kind, source_id, quote}"
    kind, source_id, quote = evidence["kind"], evidence["source_id"], evidence["quote"]
    if kind not in EVIDENCE_KINDS or not isinstance(quote, str) or len(quote) > QUOTE_MAX_CHARS:
        return "evidence kind outside the enum or quote not a string within the limit"
    if kind == "none":
        return None if quote == "" and source_id is None else "evidence none needs an empty quote and null source_id"
    if not quote.strip():
        return "evidence quote must be nonblank"
    if kind == "reference_atom":
        if source_id is not None:
            return "reference_atom evidence needs a null source_id"
        return None if any(quote in text for text in _strings(reference)) else "quote is not in any reference field"
    session = next((s for s in sources if s["source_id"] == source_id), None)
    if session is None:
        return "source evidence names an unknown source_id"
    return None if any(quote in t["content"] for t in session["turns"]) else "quote is not in that source"


def _rule_problem(data, judge_score):
    agrees, correction = data["agrees_with_judge"], data["proposed_correction"]
    if type(agrees) is not bool or correction not in CORRECTIONS or data["critical_error"] not in CRITICAL:
        return "agrees_with_judge, proposed_correction or critical_error outside the schema"
    if agrees and (correction != "keep" or judge_score == "invalid_judge"):
        return "agreement needs proposed_correction keep and a scored judge verdict"
    if not agrees and (correction == "keep" or correction == judge_score):
        return "disagreement needs a proposed score that differs from the judge's"
    rationale = data["rationale"]
    if not isinstance(rationale, str) or not rationale.strip() or len(rationale) > RATIONALE_MAX_CHARS:
        return f"rationale must be nonblank and at most {RATIONALE_MAX_CHARS} characters"
    return None


def parse_review(text, judge_score, reference, sources):
    """Strict five-field review; anything refused is ``invalid_review`` (recorded, not a stop)."""
    try:
        data = json.loads(text, object_pairs_hook=_unique_keys)
    except (TypeError, ValueError) as exc:
        return _invalid(f"review is not JSON: {exc}")
    if not isinstance(data, dict) or set(data) != REVIEW_KEYS:
        return _invalid(f"review must be an object with exactly {sorted(REVIEW_KEYS)}")
    problem = _rule_problem(data, judge_score) or _evidence_problem(data["evidence"], reference, sources)
    if problem:
        return _invalid(problem)
    return Review(True, data["agrees_with_judge"], data["proposed_correction"], data["critical_error"],
                  dict(data["evidence"]), data["rationale"], None)


def _unique_keys(pairs):
    out = {}
    for key, value in pairs:
        if key in out:
            raise ValueError("duplicate key in review")
        out[key] = value
    return out


# -- one paid review -------------------------------------------------------------------
def _hard_limited(precount, limit):
    """The exact pre-send count, with the standard-tier hard limit checked before the R_i check."""
    def count(wire):
        counted = precount(wire)
        if type(counted) is int and counted > limit:
            raise InputTokensBreach(f"review input counts {counted} tokens, above the {limit}-token standard-tier "
                                    "hard limit; nothing reserved or sent")
        return counted
    return count


def review_maximum(sheet, campaign, holdout, count):
    """(per-call maximum, count x per-call) in USD for the review bound under the campaign's policy."""
    bound = sheet.stage_profile(REVIEW_STAGE, holdout.sources_bytes_max(), True)
    per_call = sheet.max_usd(bound, campaign.policy)
    return per_call, per_call * count


def review_bound(deps):
    bound = deps.sheet.stage_profile(REVIEW_STAGE, deps.sources_bytes_max, True)
    limit = deps.sheet.standard_tier_limit(bound.model)
    if bound.r_tokens > limit:
        raise InputTokensBreach(f"review R_i {bound.r_tokens} exceeds the {limit}-token standard tier: refused")
    return bound, limit


def review_pair(deps, holdout, ctx, arm, verdict_record, plan):
    """The review record for one (question, arm): request, usage, observed model and the parsed review."""
    if deps.precount is None:
        raise RunStopped("review requires the exact pre-send input count (precount)")
    qid = ctx["question_id"]
    caps = deps.sheet.caps()
    parts = review_parts(holdout, qid, verdict_record, caps)
    bound, limit = review_bound(deps)
    completion = paid_completion(
        deps.campaign, deps.journal, deps.sheet, letter=LETTER, stage=REVIEW_STAGE,
        binding=stage_binding(deps, ctx, arm, REVIEW_STAGE, REVIEW_PROMPT_SHA256),
        messages=review_messages(parts), response_format=review_response_format(),
        transport=deps.transport, bound=bound, acknowledged=deps.acknowledged, clock=deps.clock,
        precount=_hard_limited(deps.precount, limit), retry_rejected=deps.retry_rejected)
    record = base_record(ctx, arm, verdict_record, plan)
    record.update(request_id=completion.request_id, usage=completion.usage, model_observed=completion.model,
                  outcome=completion.outcome)
    if completion.outcome == "length":
        record["status"], record["review"] = "length", _invalid("review output truncated").to_dict()
        return record
    review = parse_review(completion.text, verdict_record["score"], parts["reference"], parts["source_conversations"])
    record["status"] = "reviewed" if review.valid else "invalid_review"
    record["review"] = review.to_dict()
    return record


def base_record(ctx, arm, verdict_record, plan):
    return {"question_id": ctx["question_id"], "history_id": ctx["history_id"], "arm": arm,
            "review_list_sha256": plan.sha256, "review_prompt_sha256": plan.prompt_sha256,
            "judge": {"request_id": verdict_record.get("request_id"), "score": verdict_record["score"]},
            "request_id": None, "usage": None, "model_observed": None, "outcome": None,
            "status": None, "review": None}


# -- durable records -------------------------------------------------------------------
def review_path(campaign_dir, question_id, arm):
    return Path(campaign_dir) / "reviews" / f"{question_id}.{arm}.json"


def write_review(campaign_dir, record):
    path = review_path(campaign_dir, record["question_id"], record["arm"])
    write_create_only(path, record)
    return path


def reviewable_verdicts(directory, plan):
    """Every listed pair must have a judged candidate; refused (before any request) otherwise."""
    out, problems = {}, []
    for qid, arm in plan.pairs:
        record = read_verdict(directory, qid, arm)
        if record is None:
            problems.append(f"{qid}/{arm}: no verdict")
        elif not isinstance(record.get("candidate"), str) or record.get("score") not in REVIEWABLE_SCORES:
            problems.append(f"{qid}/{arm}: score {record.get('score')!r} has no reviewable candidate")
        else:
            out[(qid, arm)] = record
    if problems:
        raise ReviewListError(f"{len(problems)} listed pair(s) are not reviewable: {problems[:3]}")
    return out


# -- the phase ---------------------------------------------------------------------------
def phase_review(ctx):
    from paid_phases import _context_path, _deps, _minimal_context, _read
    plan = ctx.review
    if plan is None:
        raise RunStopped("--phase review needs --review-list, --review-list-sha256 and --review-prompt-sha256")
    if ctx.precount is None:
        raise RunStopped("review requires the exact pre-send input count (precount)")
    verdicts = reviewable_verdicts(ctx.directory, plan)
    freeze(ctx.directory, plan, ctx.clock)
    deps, acked, done = _deps(ctx), ctx.acknowledged_bindings(), 0
    for qid, arm in plan.pairs:
        if review_path(ctx.directory, qid, arm).exists():
            continue
        hid = ctx.holdout.questions[qid]["history_id"]
        context = _read(_context_path(ctx, qid)) if _context_path(ctx, qid).exists() else _minimal_context(ctx, qid)
        rid = acked.get((REVIEW_STAGE, hid, qid, arm))
        if rid is not None:
            record = base_record(context, arm, verdicts[(qid, arm)], plan)
            record.update(request_id=rid, status="technical_error", outcome=f"acknowledged_uncertain:{rid}")
            write_review(ctx.directory, record)
            continue
        check_cap(ctx.journal.state, plan, qid, arm)
        ctx.set_count_context(stage=REVIEW_STAGE, arm=arm, question_id=qid, history_id=hid, session_id=None,
                              r_tokens=None)
        try:
            record = review_pair(deps, ctx.holdout, context, arm, verdicts[(qid, arm)], plan)
        except ContextError as exc:  # a byte cap: this pair's technical error, not a run stop
            record = base_record(context, arm, verdicts[(qid, arm)], plan)
            record.update(status="technical_error", outcome=f"ContextError: {exc}")
        write_review(ctx.directory, record)
        done += 1
    ctx.emit(f"review: {done} review(s) this invocation; frozen list {plan.sha256[:12]} ({len(plan)} pair(s))")
