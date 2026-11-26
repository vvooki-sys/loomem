"""The judge stage: the ONLY module that reads gold (``Holdout.judge_view``).

One judge call per (question, arm) -- never deduplicated, even for identical
candidates: the binding carries the arm, so two arms with the same answer are
two request ids. The reference is the four free-text gold fields as one JSON
object; ``source_conversations`` is every session of the question's history
in plan.json shape; the user payload key order comes from the pinned judge
prompt. The parser is strict (exactly four fields) and anything it refuses
scores ``invalid_judge``. A technically failed arm is not judged at all.
"""
from dataclasses import asdict, dataclass
import json
from pathlib import Path

from paid_arms import ARMS, stage_binding
from paid_call_journal import write_create_only
from paid_context import ContextError, byte_caps_ok, byte_size
from paid_openai_wire import paid_completion

SOURCE_SCOPE = {"complete_for_fixture": True, "kind": "complete_synthetic_conversations",
                "outside_world_exhaustive": False}
REFERENCE_FIELDS = ("reference_answer", "required_answer_atoms", "equivalence_rule", "unanswerable_explanation")
VERDICT_KEYS = frozenset({"status", "correct", "answer_span", "reason"})
SCORED = "scored"
REASON_MAX_CHARS = 600


@dataclass
class Verdict:
    valid: bool
    status: str | None
    correct: bool | None
    answer_span: str | None
    reason: str | None
    error: str | None

    def to_dict(self):
        return asdict(self)


def _invalid(error, status=None):
    return Verdict(False, status, None, None, None, error)


def reference_payload(gold, cap_bytes=None):
    """The judge's reference: the four free-text gold fields (never ``category``)."""
    if not isinstance(gold, dict) or any(key not in gold for key in REFERENCE_FIELDS):
        raise ContextError(f"gold view must carry {REFERENCE_FIELDS}")
    reference = {key: gold[key] for key in REFERENCE_FIELDS}
    if cap_bytes is not None and byte_size(reference) > cap_bytes:
        raise ContextError(f"reference payload is {byte_size(reference)} bytes, above {cap_bytes}")
    return reference


def gold_reference(holdout, question_id, cap_bytes=None):
    """The reference of one question for the judge and the plan §8C review (``paid_review``): this module
    stays the only reader of ``Holdout.judge_view``."""
    return reference_payload(holdout.judge_view(question_id), cap_bytes)


def source_conversations(history):
    """All sessions of the history, plan.json shape: conversation_date, source_id (= session_id), turns."""
    sessions = history.get("sessions") if isinstance(history, dict) else None
    if not isinstance(sessions, list) or not sessions:
        raise ContextError("history without sessions")
    out = []
    for session in sessions:
        turns = [{"role": t["role"], "content": t["content"]} for t in session["turns"]]
        out.append({"conversation_date": session["conversation_date"], "source_id": session["session_id"],
                    "turns": turns})
    return out


def judge_messages(prompts, question, reference, candidate, sources, source_scope=SOURCE_SCOPE):
    parts = {"source_scope": source_scope, "source_conversations": sources, "question": question,
             "reference": reference, "candidate": candidate}
    return prompts.messages("judge", parts)


def _unique_pairs(pairs):
    out = {}
    for key, value in pairs:
        if key in out:
            raise ValueError("duplicate key in verdict")
        out[key] = value
    return out


def parse_verdict(text, candidate, statuses=None):
    """Strict four-field verdict; ``statuses`` (the pinned schema's enum) restricts ``status``."""
    if not isinstance(candidate, str):
        return _invalid("candidate must be a string")
    try:
        data = json.loads(text, object_pairs_hook=_unique_pairs)
    except (TypeError, ValueError) as exc:
        return _invalid(f"verdict is not JSON: {exc}")
    if not isinstance(data, dict) or set(data) != VERDICT_KEYS:
        return _invalid("verdict must be an object with exactly status, correct, answer_span, reason")
    status, correct, span, reason = data["status"], data["correct"], data["answer_span"], data["reason"]
    if not isinstance(status, str) or not status or (statuses is not None and status not in statuses):
        return _invalid("verdict status outside the pinned enum")
    if not isinstance(reason, str) or not reason.strip() or len(reason) > REASON_MAX_CHARS:
        return _invalid(f"verdict reason must be nonblank and at most {REASON_MAX_CHARS} characters", status)
    if not isinstance(span, str):
        return _invalid("answer_span must be a string", status)
    if status == SCORED:
        if type(correct) is not bool:
            return _invalid("scored verdict requires a boolean correct", status)
        if not candidate.strip():
            if span != "" or correct is not False:
                return _invalid("blank candidate must score incorrect with an empty answer_span", status)
        elif not span.strip() or span not in candidate:
            return _invalid("answer_span must be a nonblank exact substring of the candidate", status)
    elif correct is not None:
        return _invalid("correct must be null unless the verdict is scored", status)
    return Verdict(True, status, correct, span, reason, None)


def score(verdict):
    if not verdict.valid or verdict.status != SCORED:
        return "invalid_judge"
    return "correct" if verdict.correct else "incorrect"


def _status_enum(prompts):
    schema = prompts.stage("judge").response_schema or {}
    enum = ((schema.get("properties") or {}).get("status") or {}).get("enum")
    return tuple(enum) if isinstance(enum, list) and enum else None


def judge_arm(deps, holdout, ctx, arm, outcome):
    """Verdict record for one arm outcome: {question_id, arm, candidate, verdict, score, request_id,
    usage, outcome}. An arm that did not end ``ok`` is not judged (score = its status)."""
    if arm not in ARMS or outcome.arm != arm or outcome.question_id != ctx["question_id"]:
        raise ContextError("judge_arm: outcome does not belong to this question/arm")
    record = {"question_id": ctx["question_id"], "arm": arm, "candidate": outcome.answer, "verdict": None,
              "score": None, "request_id": None, "usage": None, "outcome": outcome.status}
    if outcome.status != "ok" or not isinstance(outcome.answer, str):
        record["score"] = outcome.status if outcome.status != "ok" else "technical_error"
        return record
    caps = deps.sheet.caps()
    reference = reference_payload(holdout.judge_view(ctx["question_id"]), caps["QUESTION_CAP"])
    sources = source_conversations(holdout.histories[ctx["history_id"]])
    parts = {"source_scope": SOURCE_SCOPE, "source_conversations": sources, "question": ctx["question"],
             "reference": reference, "candidate": outcome.answer}
    byte_caps_ok("judge", parts, caps)
    prompts = deps.prompts
    completion = paid_completion(
        deps.campaign, deps.journal, deps.sheet, letter=ARMS[arm], stage="judge",
        binding=stage_binding(deps, ctx, arm, "judge", prompts.stage("judge").sha256),
        messages=prompts.messages("judge", parts), response_format=prompts.response_format("judge"),
        transport=deps.transport, bound=deps.sheet.stage_profile("judge", deps.sources_bytes_max, True),
        acknowledged=deps.acknowledged, clock=deps.clock, precount=deps.precount,
        retry_rejected=deps.retry_rejected)
    record["request_id"], record["usage"] = completion.request_id, completion.usage
    if completion.outcome == "length":
        verdict = _invalid("judge output truncated (finish_reason=length or output-limit HTTP 400)")
    else:
        verdict = parse_verdict(completion.text, outcome.answer, _status_enum(prompts))
    record["verdict"], record["score"] = verdict.to_dict(), score(verdict)
    return record


# -- durable records ---------------------------------------------------------
def verdict_path(campaign_dir, question_id, arm):
    return Path(campaign_dir) / "verdicts" / f"{question_id}.{arm}.json"


def write_verdict(campaign_dir, record):
    path = verdict_path(campaign_dir, record["question_id"], record["arm"])
    write_create_only(path, record)
    return path


def read_verdict(campaign_dir, question_id, arm):
    path = verdict_path(campaign_dir, question_id, arm)
    if not path.is_file():
        return None
    return json.loads(path.read_text(encoding="utf-8"))
