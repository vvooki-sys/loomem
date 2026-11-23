"""Shared offline fixtures for the paid-runner tests (not a test module itself).

Everything is synthetic and lives in a temporary directory: a fixture cost
sheet with the same API as the validation sheet, a valid campaign spec, a
miniature plan.json with the five stage templates (synthetic prompt text, NOT
the frozen prompts), a miniature holdout with its manifest, and a recording
fake transport. No network, no real prompts, no gold from the real holdout.
"""
import hashlib
import json
from pathlib import Path

from paid_campaign import create_campaign
from paid_price_sheet import load_sheet

# Same API and the same published numbers as the validation sheet so money
# assertions (judge P1 = 1.31892 USD, P2 = 15.712 USD) can be checked exactly.
# "gpt-6.1-sol" has no published window and no long-context price (PriceUnknown).
SHEET_SOURCE = '''
FRAMING = 8192
PRICES = {
    "gpt-6-luna": (0.10, 0.50, 272_000, 0.20, 0.75),
    "gpt-6.1-sol": (2.00, 10.00, 272_000, None, None),
    "gpt-6.2-astra": (8.00, 40.00, 272_000, 16.00, 60.00),
}
CACHED_INPUT = {"gpt-6-luna": (0.01, 0.02, 0.005, 0.01)}
CONTEXT_WINDOW = {"gpt-6-luna": 1_050_000, "gpt-6.1-sol": None, "gpt-6.2-astra": 922_000}
JUDGE_MODEL = "gpt-6.2-astra"
JUDGE_OUT = {"judge": 16_000, "review": 16_000}
FACTS_JSON_CAP = 32_768
EVIDENCE_CAP = 32_768
DIAG_CAP = 4_096
ANSWER_CAP = 4_096
ORIGINALS_MENU_CAP = 65_536
QUESTION_CAP = 2_048
SYSTEM = {"diagnostic": 1_600, "audit": 2_400, "selection": 3_000, "answer": 800,
          "judge": 3_200, "review": 4_000, "extraction": 6_000}
OUT = {"extraction": 2_000, "diagnostic": 1_024, "audit": 512, "selection": 512,
       "answer": 1_024, "judge": 512, "review": 1_024}
E_CHUNK_TOKENS = 4000
E_CHUNK_OUT = 16_384
E_CALLS_PER_CHUNK = 5
MODEL = {"extraction": "gpt-6-luna", "diagnostic": "gpt-6-luna", "audit": "gpt-6-luna",
         "selection": "gpt-6-luna", "answer": "gpt-6-luna", "judge": JUDGE_MODEL, "review": JUDGE_MODEL}


def cost(model, r, o):
    pin, pout, thr, lin, lout = PRICES[model]
    if r > thr:
        if lin is None:
            return None
        pin, pout = lin, lout
    return r / 1e6 * pin + o / 1e6 * pout


def bounds(sources_bytes_max, judge_sees_sources):
    stat_q = QUESTION_CAP
    return {
        "diagnostic": SYSTEM["diagnostic"] + stat_q + FACTS_JSON_CAP + FRAMING,
        "audit": SYSTEM["audit"] + stat_q + FACTS_JSON_CAP + 6 * DIAG_CAP + FRAMING,
        "selection": SYSTEM["selection"] + stat_q + FACTS_JSON_CAP + ORIGINALS_MENU_CAP + 6 * DIAG_CAP + FRAMING,
        "answer": SYSTEM["answer"] + stat_q + 6 * (EVIDENCE_CAP + DIAG_CAP) + FRAMING,
        "judge": SYSTEM["judge"] + 2 * stat_q + 6 * ANSWER_CAP + FRAMING
        + (sources_bytes_max if judge_sees_sources else 0),
        "review": SYSTEM["review"] + 2 * stat_q + 6 * ANSWER_CAP + FRAMING + sources_bytes_max,
    }
'''
SOURCES_BYTES_MAX = 44_801   # gives judge R_i = 84_865 as on the validation sheet


def sha256_bytes(data):
    return hashlib.sha256(data).hexdigest()


def write_sheet(directory, source=SHEET_SOURCE):
    path = Path(directory) / "cost_sheet_fixture.py"
    path.write_text(source)
    return path, sha256_bytes(path.read_bytes())


def load_fixture_sheet(directory):
    path, digest = write_sheet(directory)
    return load_sheet(path, digest)


PROFILES = {
    "extraction": {"model": "gpt-6-luna", "reasoning_effort": "low", "max_completion_tokens": 16384},
    "control": {"model": "gpt-6.2-astra", "reasoning_effort": "high", "max_completion_tokens": 4096},
    "diagnostic": {"model": "gpt-6-luna", "reasoning_effort": "low", "max_completion_tokens": 1024},
    "audit": {"model": "gpt-6-luna", "reasoning_effort": "low", "max_completion_tokens": 512},
    "selection": {"model": "gpt-6-luna", "reasoning_effort": "low", "max_completion_tokens": 512},
    "answer": {"model": "gpt-6-luna", "reasoning_effort": "low", "max_completion_tokens": 1024},
    "judge": {"model": "gpt-6.2-astra", "reasoning_effort": "high", "max_completion_tokens": 16000},
    "review": {"model": "gpt-6.2-astra", "reasoning_effort": "high", "max_completion_tokens": 16000},
}
ZERO64 = "0" * 64


def spec(**overrides):
    base = {
        "schema": 1, "mode": "paid", "created_at": "2026-11-22T00:00:00Z",
        "limit_usd": "100", "sublimits_usd": {"E": "10", "F": "30", "S": "30", "A": "10", "R": "10", "K": "0.2374"},
        "bound_policy": "p1", "p1_framing_accepted": True,
        "cost_sheet": {"path": "fixture", "sha256": ZERO64},
        "plan_json": {"path": "fixture", "sha256": ZERO64, "stage_sha256": {}},
        "holdout": {"dir": "fixture", "manifest_sha256": ZERO64, "always50_sha256": ZERO64},
        "profiles": json.loads(json.dumps(PROFILES)),
        "provider": {"base_url": "http://127.0.0.1:1/v1", "api_key_env": "PAID_TEST_KEY", "allowed_host": "127.0.0.1"},
        "timeout_s": 5, "judge_sees_sources": True,
    }
    base.update(overrides)
    return base


def make_campaign(directory, sheet=None, **overrides):
    return create_campaign(Path(directory) / "campaign", spec(**overrides), sheet)


def messages(stage="diagnostic", user=None):
    return [{"role": "system", "content": f"synthetic {stage} system"},
            {"role": "user", "content": user if user is not None else json.dumps({"question": "Where?", "facts": []})}]


def binding(qid="q1", arm="facts_retry", stage="diagnostic", attempt=1):
    return {"history_id": "h1", "session_id": None, "question_id": qid, "arm": arm, "stage": stage,
            "attempt": attempt, "manifest_sha256": ZERO64, "context_sha256": ZERO64, "prompt_sha256": ZERO64}


def ok_body(model="gpt-6-luna", text="Synthetic answer.", prompt=100, completion=10, finish="stop", **extra):
    body = {"id": "chatcmpl-1", "object": "chat.completion", "model": model,
            "choices": [{"index": 0, "finish_reason": finish, "message": {"role": "assistant", "content": text}}],
            "usage": {"prompt_tokens": prompt, "completion_tokens": completion, "total_tokens": prompt + completion}}
    body.update(extra)
    return json.dumps(body).encode("utf-8")


class FakeTransport:
    """Records every call; replies from a FIFO queue of (status, headers, body) or raises.

    With ``journal_dir`` it probes, at call time, whether the request's
    ``<rid>.sent.json`` exists (``saw_sent``) and whether ``budget.json`` holds
    a reservation for that rid (``saw_reserved``). ``host`` is loopback so a
    paid-mode test campaign admits it without a real provider.
    """
    host = "127.0.0.1"

    def __init__(self, replies=None, journal_dir=None, host=None):
        self.replies = list(replies or [])
        self.calls = []
        self.journal_dir = Path(journal_dir) if journal_dir else None
        self.saw_sent = []
        self.saw_reserved = []
        if host is not None:
            self.host = host

    def __call__(self, body, timeout_s):
        self.calls.append((body, timeout_s))
        if self.journal_dir is not None:
            wire_sha = sha256_bytes(body)
            intents = {p.name.split(".")[0]: json.loads(p.read_text())
                       for p in (self.journal_dir / "calls").glob("*.intent.json")}
            rids = [rid for rid, intent in intents.items() if intent["wire_sha256"] == wire_sha]
            self.saw_sent.append(any((self.journal_dir / "calls" / f"{rid}.sent.json").exists() for rid in rids))
            ledger = json.loads((self.journal_dir / "budget.json").read_text())["requests"]
            self.saw_reserved.append(any(rid in ledger and ledger[rid]["actual_usd"] is None for rid in rids))
        if not self.replies:
            raise AssertionError("fake transport called without a queued reply")
        reply = self.replies.pop(0)
        if isinstance(reply, BaseException):
            raise reply
        return reply


# -- miniature plan.json --------------------------------------------------
STAGE_SCHEMAS = {
    "diagnostic": {"type": "object", "properties": {"answer": {"type": "string"}, "coverage": {"type": "string"},
                                                    "missing_parts": {"type": "array"}, "conflict_fact_ids": {"type": "array"},
                                                    "supporting_fact_ids": {"type": "array"}},
                   "required": ["answer", "coverage", "missing_parts", "conflict_fact_ids", "supporting_fact_ids"],
                   "additionalProperties": False},
    "audit": {"type": "object", "properties": {"claims_entailed": {"type": "boolean"}, "issues": {"type": "array"}},
              "required": ["claims_entailed", "issues"], "additionalProperties": False},
    "selection": {"type": "object", "properties": {"selected_ids": {"type": "array", "maxItems": 16}},
                  "required": ["selected_ids"], "additionalProperties": False},
    "answer": None,
    "judge": {"type": "object", "properties": {"status": {"type": "string"}, "correct": {"type": ["boolean", "null"]},
                                               "answer_span": {"type": "string"}, "reason": {"type": "string"}},
              "required": ["status", "correct", "answer_span", "reason"], "additionalProperties": False},
}
STAGE_USER = {
    "diagnostic": {"facts": [], "question": "q"},
    "audit": {"facts": [], "proposed_answer": "a", "question": "q"},
    "selection": {"diagnostic": {}, "facts": [], "original_records": [], "question": "q"},
    "answer": {"evidence": {}, "first_diagnostic": {}, "question": "q"},
    "judge": {"candidate": "c", "question": "q", "reference": "r", "source_conversations": [], "source_scope": {}},
}


def plan_document(calls_per_stage=2):
    calls = []
    for stage, user in STAGE_USER.items():
        for i in range(calls_per_stage):
            calls.append({"id": f"q{i}:{stage}", "stage": stage, "qid": f"q{i}",
                          "template": {"messages": [{"role": "system", "content": f"SYNTHETIC {stage} SYSTEM"},
                                                    {"role": "user", "content": json.dumps(user, separators=(",", ":"))}],
                                       "response_schema": STAGE_SCHEMAS[stage]}})
    return {"schema_version": 1, "calls": calls}


def write_plan(directory, document=None):
    path = Path(directory) / "plan.json"
    path.write_bytes(json.dumps(document or plan_document(), sort_keys=True).encode("utf-8"))
    return path, sha256_bytes(path.read_bytes())


def write_judge_prompt(directory, plan_sha256, order=None, **overrides):
    doc = {"name": "judge-prompt-fixture", "derived_from": {"sha256": plan_sha256},
           "system": "SYNTHETIC judge SYSTEM", "response_schema": STAGE_SCHEMAS["judge"],
           "user_payload_field_order": order or ["source_scope", "source_conversations", "question", "reference", "candidate"]}
    doc.update(overrides)
    path = Path(directory) / "judge_prompt.json"
    path.write_bytes(json.dumps(doc, sort_keys=True).encode("utf-8"))
    return path, sha256_bytes(path.read_bytes())


# -- miniature holdout -----------------------------------------------------
def question_row(qid, hid, question_date="2026-04-02"):
    return {"question_id": qid, "history_id": hid, "category": "temporal_update",
            "question_date": question_date, "question": f"Synthetic question {qid}?",
            "reference_answer": f"GOLD-REFERENCE-{qid}-SECRET", "required_answer_atoms": [f"GOLD-ATOM-{qid}-SECRET"],
            "equivalence_rule": f"GOLD-RULE-{qid}-SECRET", "evidence": [], "critical_risk": "",
            "unanswerable_explanation": None, "author_notes": f"GOLD-NOTES-{qid}-SECRET"}


def history_doc(hid, sessions=2):
    return {"history_id": hid, "domain": "synthetic",
            "sessions": [{"session_id": f"{hid}-s{i + 1}", "conversation_date": f"2025-07-{10 + i:02d}",
                          "turns": [{"role": "user", "content": f"Synthetic turn {i} of {hid}."},
                                    {"role": "assistant", "content": "Noted."}]} for i in range(sessions)]}


def write_holdout(directory, histories=("h1",), questions_per_history=2, always=("h1-q01",)):
    root = Path(directory) / "holdout"
    (root / "shards/a/histories").mkdir(parents=True)
    files, order, q2h = {}, [], {}
    lines = []
    for hid in histories:
        path = root / f"shards/a/histories/{hid}.json"
        path.write_text(json.dumps(history_doc(hid)))
        files[f"shards/a/histories/{hid}.json"] = sha256_bytes(path.read_bytes())
        for i in range(questions_per_history):
            qid = f"{hid}-q{i + 1:02d}"
            lines.append(json.dumps(question_row(qid, hid)))
            order.append(qid)
            q2h[qid] = hid
    qpath = root / "shards/a/questions.jsonl"
    qpath.write_text("\n".join(lines) + "\n")
    files["shards/a/questions.jsonl"] = sha256_bytes(qpath.read_bytes())
    tooling = root / "SPEC.md"
    tooling.write_text("synthetic spec\n")
    apath = root / "always_originals_50.json"
    apath.write_text(json.dumps({"question_ids": list(always), "seed": 1}))
    always_sha = sha256_bytes(apath.read_bytes())
    manifest = {"name": "synthetic-holdout", "histories": len(histories), "questions": len(order),
                "ordered_question_ids": order, "question_to_history": q2h, "files_sha256": files,
                "fix_logs_sha256": {}, "reviews_sha256": {},
                "tooling_sha256": {"SPEC.md": sha256_bytes(tooling.read_bytes())},
                "always_originals_50": {"file": "always_originals_50.json", "question_ids": list(always), "sha256": always_sha}}
    mpath = root / "manifest.json"
    mpath.write_bytes(json.dumps(manifest, sort_keys=True).encode("utf-8"))
    return root, sha256_bytes(mpath.read_bytes()), always_sha
