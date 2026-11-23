"""The three execution arms over one gold-blind question context (plan §3).

F = ``facts_retry``        diagnostic -> answer
S = ``selective_originals`` diagnostic -> audit -> selection -> answer (ALWAYS all four;
                            the audit verdict is recorded as ``need``, never branched on)
A = ``always_originals``   diagnostic -> answer with ALL menu originals in engine rank order

Every stage is one ``paid_completion`` whose user payload keys are exactly the
pinned plan's (``Prompts.messages`` enforces the key set), whose binding carries
the context hash and the stage prompt hash, and whose letter is the arm's. A
``length`` outcome or an unparsable/invalid stage output ends the arm as a
technical outcome (later stages skipped, no judge). Fail-closed stops
(``RunStopped`` family, ``BudgetError``, ``SublimitExceeded``, ``CampaignError``)
propagate untouched. Nothing here sees a gold field: only ``ctx`` fields.
"""
from dataclasses import asdict, dataclass, field
import json
from pathlib import Path
import time

from evidence_gate import normalize_diagnostic
from gate_verifier import verified_need
from paid_call_journal import write_create_only
from paid_context import ContextError, MAX_SELECTED, byte_caps_ok, byte_size, pack_evidence
from paid_openai_wire import paid_completion

ARMS = {"facts_retry": "F", "selective_originals": "S", "always_originals": "A"}
ARM_STAGES = {"facts_retry": ("diagnostic", "answer"),
              "selective_originals": ("diagnostic", "audit", "selection", "answer"),
              "always_originals": ("diagnostic", "answer")}
STATUSES = ("ok", "technical_error", "length", "not_started_budget")


class StageFailure(Exception):
    """A stage answered, but its output cannot be used (unparsable, invalid, truncated)."""

    def __init__(self, status, stage, detail):
        super().__init__(f"{stage}: {detail}")
        self.status, self.stage, self.detail = status, stage, detail


@dataclass
class ArmDeps:
    campaign: object
    journal: object
    sheet: object
    prompts: object
    transport: object
    sources_bytes_max: int
    manifest_sha256: str
    acknowledged: frozenset = frozenset()
    precount: object = None
    clock: object = time.time


@dataclass
class ArmOutcome:
    question_id: str
    arm: str
    status: str
    answer: str | None = None
    stages: dict = field(default_factory=dict)
    diagnostic: dict | None = None
    need: dict | None = None
    selected_ids: list = field(default_factory=list)
    rejected_ids: list = field(default_factory=list)
    zero_facts: bool = False
    error: str | None = None

    def to_dict(self):
        return json.loads(json.dumps(asdict(self), allow_nan=False))

    @classmethod
    def from_dict(cls, data):
        if not isinstance(data, dict) or set(data) != set(cls.__dataclass_fields__):
            raise ValueError("arm outcome dict must carry exactly the ArmOutcome fields")
        if data["status"] not in STATUSES or data["arm"] not in ARMS:
            raise ValueError("arm outcome with unknown status or arm")
        return cls(**data)


def stage_binding(deps, ctx, arm, stage, prompt_sha256, attempt=1):
    return {"history_id": ctx["history_id"], "session_id": None, "question_id": ctx["question_id"],
            "arm": arm, "stage": stage, "attempt": attempt, "manifest_sha256": deps.manifest_sha256,
            "context_sha256": ctx["context_sha256"], "prompt_sha256": prompt_sha256}


def call_stage(deps, ctx, arm, stage, parts, attempt=1):
    """One bounded paid call for a stage; raises the fail-closed stops of paid_completion."""
    if arm not in ARMS:
        raise ContextError(f"unknown arm {arm!r}")
    byte_caps_ok(stage, parts, deps.sheet.caps())
    prompts = deps.prompts
    return paid_completion(deps.campaign, deps.journal, deps.sheet, letter=ARMS[arm], stage=stage,
                           binding=stage_binding(deps, ctx, arm, stage, prompts.stage(stage).sha256, attempt),
                           messages=prompts.messages(stage, parts), response_format=prompts.response_format(stage),
                           transport=deps.transport, bound=deps.sheet.stage_profile(stage, deps.sources_bytes_max, True),
                           acknowledged=deps.acknowledged, clock=deps.clock, precount=deps.precount)


def _stage_record(completion):
    return {"request_id": completion.request_id, "outcome": completion.outcome, "usage": completion.usage,
            "replayed": completion.replayed, "model": completion.model}


def _json_object(completion, stage):
    if completion.outcome == "length":
        raise StageFailure("length", stage, "finish_reason=length")
    try:
        data = json.loads(completion.text)
    except ValueError as exc:
        raise StageFailure("technical_error", stage, f"output is not JSON ({exc})") from exc
    if not isinstance(data, dict):
        raise StageFailure("technical_error", stage, "output is not a JSON object")
    return data


def _diagnostic(deps, ctx, arm, outcome, attempt):
    completion = call_stage(deps, ctx, arm, "diagnostic", {"facts": ctx["facts"], "question": ctx["question"]}, attempt)
    outcome.stages["diagnostic"] = _stage_record(completion)
    data = _json_object(completion, "diagnostic")
    try:
        checked, notes = normalize_diagnostic(data, ctx["facts"])
    except ValueError as exc:
        raise StageFailure("technical_error", "diagnostic", str(exc)) from exc
    outcome.diagnostic = {"diagnostic": checked, "normalization": notes}
    return checked


def _audit(deps, ctx, arm, outcome, diagnostic, attempt):
    parts = {"facts": ctx["facts"], "proposed_answer": diagnostic["answer"], "question": ctx["question"]}
    completion = call_stage(deps, ctx, arm, "audit", parts, attempt)
    outcome.stages["audit"] = _stage_record(completion)
    data = _json_object(completion, "audit")
    try:
        outcome.need = verified_need(ctx["question"], diagnostic, ctx["facts"], data)
    except ValueError as exc:
        raise StageFailure("technical_error", "audit", str(exc)) from exc


def _selection(deps, ctx, arm, outcome, diagnostic, attempt):
    parts = {"diagnostic": diagnostic, "facts": ctx["facts"], "original_records": ctx["menu"],
             "question": ctx["question"]}
    completion = call_stage(deps, ctx, arm, "selection", parts, attempt)
    outcome.stages["selection"] = _stage_record(completion)
    data = _json_object(completion, "selection")
    selected = data.get("selected_ids")
    if set(data) != {"selected_ids"} or not isinstance(selected, list) or any(not isinstance(i, str) for i in selected):
        raise StageFailure("technical_error", "selection", "selected_ids must be the only key, a list of strings")
    return selected


def _answer(deps, ctx, arm, outcome, diagnostic, evidence, attempt):
    parts = {"evidence": evidence, "first_diagnostic": diagnostic, "question": ctx["question"]}
    completion = call_stage(deps, ctx, arm, "answer", parts, attempt)
    outcome.stages["answer"] = _stage_record(completion)
    if completion.outcome == "length":
        raise StageFailure("length", "answer", "finish_reason=length")
    cap = deps.sheet.caps()["ANSWER_CAP"]
    if byte_size(completion.text) > cap:
        raise StageFailure("technical_error", "answer", f"answer longer than ANSWER_CAP {cap}")
    return completion.text


def _evidence(deps, ctx, arm, outcome, selected):
    cap = deps.sheet.caps()["EVIDENCE_CAP"]
    limit = None if arm == "always_originals" else MAX_SELECTED
    packed = pack_evidence(ctx["facts"], ctx["menu"], selected, cap, max_selected=limit)
    outcome.selected_ids = [r["record_id"] for r in packed["originals"]]
    outcome.rejected_ids = list(packed["rejected_ids"])
    return packed


def run_arm(arm, ctx, deps, attempt=1):
    """Run one arm over a ``contexts/<qid>.json`` object; returns the ArmOutcome (never raises
    for a stage's bad output; fail-closed stops of the paid call propagate)."""
    if arm not in ARMS:
        raise ContextError(f"unknown arm {arm!r}")
    outcome = ArmOutcome(ctx["question_id"], arm, "ok", zero_facts=bool(ctx.get("zero_facts")))
    try:
        diagnostic = _diagnostic(deps, ctx, arm, outcome, attempt)
        selected = []
        if arm == "selective_originals":
            _audit(deps, ctx, arm, outcome, diagnostic, attempt)
            selected = _selection(deps, ctx, arm, outcome, diagnostic, attempt)
        elif arm == "always_originals":
            selected = [r["record_id"] for r in ctx["menu"]]
        evidence = _evidence(deps, ctx, arm, outcome, selected)
        outcome.answer = _answer(deps, ctx, arm, outcome, diagnostic, evidence, attempt)
    except StageFailure as failure:
        outcome.status, outcome.error = failure.status, str(failure)
    return outcome


# -- durable records (written by the runner, read by the judge phase and the report) --
def arm_path(campaign_dir, question_id, arm):
    return Path(campaign_dir) / "arms" / f"{question_id}.{arm}.json"


def write_arm_outcome(campaign_dir, outcome):
    path = arm_path(campaign_dir, outcome.question_id, outcome.arm)
    write_create_only(path, outcome.to_dict())
    return path


def read_arm_outcome(campaign_dir, question_id, arm):
    path = arm_path(campaign_dir, question_id, arm)
    if not path.is_file():
        return None
    return ArmOutcome.from_dict(json.loads(path.read_text(encoding="utf-8")))
