"""Stage prompt templates read from a hash-pinned ``plan.json`` (never copied into code).

``plan.json`` holds one call per (question, arm, stage); for every stage the
system message, the response schema and the set of user-message keys are
identical across calls, so the five stage templates are extracted from it:
``diagnostic``, ``audit``, ``selection``, ``answer`` (no schema: plain text)
and ``judge``. The user message is a compact JSON object whose key set is
frozen; the key ORDER is the plan's raw order unless a separately frozen
judge-prompt file (``judge_prompt_v1.json``: ``user_payload_field_order`` with
sources before the candidate, for prefix caching) is pinned alongside — it must
agree with the plan on system text, schema, key set and ``derived_from.sha256``.
"""
from dataclasses import dataclass
import hashlib
import json
from pathlib import Path

STAGES = ("diagnostic", "audit", "selection", "answer", "judge")


class PromptsError(ValueError):
    """The plan or the judge-prompt file is not the pinned one or is inconsistent."""


@dataclass(frozen=True)
class StagePrompt:
    name: str
    system: str
    response_schema: dict | None
    user_keys: tuple
    sha256: str


def _sha256(data):
    return hashlib.sha256(data).hexdigest()


def _canonical(value):
    return json.dumps(value, sort_keys=True, ensure_ascii=False, separators=(",", ":"), allow_nan=False)


def _pinned_bytes(path, expected_sha256, label):
    path = Path(path)
    if not isinstance(expected_sha256, str) or len(expected_sha256) != 64:
        raise PromptsError(f"{label}: expected_sha256 must be a 64-hex SHA-256")
    if not path.is_file():
        raise PromptsError(f"{label} not found: {path}")
    data = path.read_bytes()
    actual = _sha256(data)
    if actual != expected_sha256:
        raise PromptsError(f"{label} SHA-256 mismatch: expected {expected_sha256}, got {actual}")
    return data, actual


def _extract_stages(plan):
    calls = plan.get("calls")
    if not isinstance(calls, list) or not calls:
        raise PromptsError("plan.json has no calls")
    seen = {}
    for call in calls:
        stage = call.get("stage")
        template = call.get("template") or {}
        messages = template.get("messages")
        if (stage not in STAGES or not isinstance(messages, list) or len(messages) != 2
                or messages[0].get("role") != "system" or messages[1].get("role") != "user"):
            raise PromptsError(f"call {call.get('id')!r}: unexpected stage or message shape")
        try:
            keys = tuple(json.loads(messages[1]["content"]))
        except (ValueError, TypeError) as exc:
            raise PromptsError(f"call {call.get('id')!r}: user message is not a JSON object") from exc
        triple = (messages[0]["content"], _canonical(template.get("response_schema")), keys)
        previous = seen.setdefault(stage, triple)
        if previous[0] != triple[0] or previous[1] != triple[1] or set(previous[2]) != set(keys):
            raise PromptsError(f"stage {stage!r}: more than one system/schema/key-set in plan.json")
    if set(seen) != set(STAGES):
        raise PromptsError(f"plan.json stages {sorted(seen)} differ from {STAGES}")
    return {stage: (system, json.loads(schema), keys) for stage, (system, schema, keys) in seen.items()}


class Prompts:
    def __init__(self, stages, plan_sha256, judge_sha256=None):
        self._stages = {}
        for name, (system, schema, keys) in stages.items():
            digest = _sha256(_canonical({"system": system, "response_schema": schema,
                                         "user_keys": list(keys)}).encode("utf-8"))
            self._stages[name] = StagePrompt(name, system, schema, tuple(keys), digest)
        self.plan_sha256 = plan_sha256
        self.judge_sha256 = judge_sha256

    def stage(self, name):
        if name not in self._stages:
            raise PromptsError(f"unknown stage {name!r}")
        return self._stages[name]

    def render_user(self, name, parts):
        prompt = self.stage(name)
        if not isinstance(parts, dict) or set(parts) != set(prompt.user_keys):
            raise PromptsError(f"stage {name!r}: user keys must be exactly {sorted(prompt.user_keys)}")
        ordered = {key: parts[key] for key in prompt.user_keys}
        return json.dumps(ordered, ensure_ascii=False, separators=(",", ":"), allow_nan=False)

    def messages(self, name, parts):
        return [{"role": "system", "content": self.stage(name).system},
                {"role": "user", "content": self.render_user(name, parts)}]

    def response_format(self, name):
        schema = self.stage(name).response_schema
        if schema is None:
            return None
        return {"type": "json_schema", "json_schema": {"name": name, "strict": True, "schema": schema}}

    def fingerprint(self):
        out = {name: self._stages[name].sha256 for name in STAGES}
        out["plan_json"] = self.plan_sha256
        out["judge_prompt"] = self.judge_sha256
        return out


def _apply_judge_prompt(stages, plan_sha256, judge_prompt):
    path, expected = judge_prompt
    data, digest = _pinned_bytes(path, expected, "judge prompt")
    spec = json.loads(data)
    system, schema, keys = stages["judge"]
    derived = spec.get("derived_from") or {}
    if derived.get("sha256") != plan_sha256:
        raise PromptsError("judge prompt was derived from a different plan.json")
    if spec.get("system") != system:
        raise PromptsError("judge prompt system text differs from plan.json")
    if _canonical(spec.get("response_schema")) != _canonical(schema):
        raise PromptsError("judge prompt response schema differs from plan.json")
    order = spec.get("user_payload_field_order")
    if not isinstance(order, list) or set(order) != set(keys) or len(order) != len(keys):
        raise PromptsError("judge prompt field order must be a permutation of the plan's judge keys")
    stages["judge"] = (system, schema, tuple(order))
    return digest


def load_prompts(path, expected_sha256, judge_prompt=None):
    """Load the pinned plan; optionally apply a pinned judge-prompt key order."""
    data, plan_sha256 = _pinned_bytes(path, expected_sha256, "plan.json")
    try:
        plan = json.loads(data)
    except ValueError as exc:
        raise PromptsError("plan.json is not valid JSON") from exc
    stages = _extract_stages(plan)
    judge_sha256 = _apply_judge_prompt(stages, plan_sha256, judge_prompt) if judge_prompt else None
    return Prompts(stages, plan_sha256, judge_sha256)
