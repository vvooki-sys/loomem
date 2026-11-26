"""Campaign report: states per question x arm, money per letter/stage, extraction, pins.

Pure aggregation over the campaign directory and the journal state; nothing
here calls a provider or touches money. Every state is counted separately
(``correct``, ``incorrect``, ``invalid_judge``, ``technical_error``, ``length``,
``not_started_budget``, ``zero_facts``, ``pending``; ``extraction_capped`` sessions are
counted under ``extraction`` and ``capped_questions`` and stay in every arm) against a frozen
denominator: every holdout question for F and S, the always-originals list
for A. Missing directories are tolerated (an interrupted campaign still
reports). Money is reported as exact Decimal strings: the reserved maximum
(``max_usd``) next to what was actually billed (``actual_usd``); ``length_http400``
counts the ``length`` calls that came as OpenAI's output-limit HTTP 400. ``retried`` counts the
calls sent as a later attempt of a ``rejected_4xx`` request (``run --retry-rejected``) and
``max_attempt`` is the highest attempt in the row; the rejected attempt itself stays counted in
``calls`` and ``rejected_4xx``. Like ``calls``, ``retried`` counts intents (attempt > 1), sent or not.
"""
from decimal import Decimal
import json
from pathlib import Path
import statistics

from paid_arms import ARMS
from paid_call_journal import LETTERS, write_create_only

ARM_STATES = ("correct", "incorrect", "invalid_judge", "technical_error", "length",
              "not_started_budget", "ok_unjudged", "pending")


def _read_dir(directory, name, skip=()):
    """Every ``<name>/*.json`` except dot files and names ending in one of ``skip``."""
    folder = Path(directory) / name
    if not folder.is_dir():
        return []
    out = []
    for path in sorted(folder.glob("*.json")):
        if path.name.startswith(".") or path.name.endswith(tuple(skip)):
            continue
        try:
            out.append(json.loads(path.read_text(encoding="utf-8")))
        except (OSError, ValueError):
            out.append({"_unreadable": path.name})
    return out


def _expected(holdout, contexts, arms):
    """Denominators: all questions for F/S, the always list for A (from the holdout when given)."""
    if holdout is not None:
        all_ids = list(holdout.question_order)
        always = [q for q in all_ids if q in holdout.always_ids]
    else:
        all_ids = sorted(set(contexts) | {q for q, _ in arms})
        always = sorted(q for q, arm in arms if arm == "always_originals")
    return {arm: (always if arm == "always_originals" else all_ids) for arm in ARMS}


def _state(qid, arm, arms, verdicts):
    verdict = verdicts.get((qid, arm))
    if verdict is not None and verdict.get("score") is not None:
        return verdict["score"]
    outcome = arms.get((qid, arm))
    if outcome is None:
        return "pending"
    return "ok_unjudged" if outcome.get("status") == "ok" else outcome.get("status", "pending")


def _arm_table(expected, contexts, arms, verdicts):
    per_arm, states = {}, {}
    for arm, ids in expected.items():
        row = {state: 0 for state in ARM_STATES}
        row.update(expected=len(ids), judged=0, zero_facts=0, empty_retrieval=0, context_missing=0)
        for qid in ids:
            state = _state(qid, arm, arms, verdicts)
            row[state] = row.get(state, 0) + 1
            states.setdefault(qid, {})[arm] = state
            verdict = verdicts.get((qid, arm))
            if verdict is not None and verdict.get("request_id") is not None:
                row["judged"] += 1
            context = contexts.get(qid)
            if context is None:
                row["context_missing"] += 1
            elif context.get("zero_facts"):
                row["zero_facts"] += 1          # extraction produced no fact for the history
            elif context.get("empty_retrieval"):
                row["empty_retrieval"] += 1     # facts exist, retrieval returned none for this question
        row["accuracy_over_expected"] = str(Decimal(row["correct"]) / Decimal(len(ids))) if ids else None
        per_arm[arm] = row
    return per_arm, states


def _empty_row():
    return {"calls": 0, "settled": 0, "reserved_max_usd": Decimal(0), "actual_usd": Decimal(0),
            "rejected_4xx": 0, "uncertain": 0, "model_drift": 0, "bound_breach": 0, "length": 0, "length_http400": 0,
            "void": 0, "retried": 0, "max_attempt": 0}


def _add(row, intent, settled, uncertain, void):
    row["calls"] += 1
    attempt = (intent.get("binding") or {}).get("attempt")
    attempt = attempt if type(attempt) is int else 1
    row["retried"] += attempt > 1       # a later attempt of a rejected_4xx request (run --retry-rejected)
    row["max_attempt"] = max(row["max_attempt"], attempt)
    row["reserved_max_usd"] += Decimal(intent["max_usd"])
    row["uncertain"] += bool(uncertain)
    row["void"] += bool(void)
    if settled is None:
        return
    row["settled"] += 1
    if settled.get("actual_usd") is not None:
        row["actual_usd"] += Decimal(settled["actual_usd"])
    outcome = settled.get("outcome")
    row["rejected_4xx"] += outcome == "rejected_4xx"
    row["model_drift"] += outcome == "model_drift"
    row["length"] += outcome == "length"
    row["length_http400"] += outcome == "length" and settled.get("http_status") == 400  # output-limit 400
    usage = settled.get("usage") or {}
    if usage and (usage.get("prompt_tokens", 0) > intent.get("r_tokens", float("inf"))
                  or usage.get("completion_tokens", 0) > intent.get("o_tokens", float("inf"))):
        row["bound_breach"] += 1


def _money_rows(state):
    by_letter = {letter: _empty_row() for letter in LETTERS}
    by_stage, by_letter_stage = {}, {}
    for rid, intent in sorted(state.intents.items()):
        settled, uncertain, void = state.settled.get(rid), rid in state.uncertain, rid in state.void
        letter, stage = intent["letter"], intent["stage"]
        for row in (by_letter.setdefault(letter, _empty_row()), by_stage.setdefault(stage, _empty_row()),
                    by_letter_stage.setdefault(f"{letter}:{stage}", _empty_row())):
            _add(row, intent, settled, uncertain, void)
    return {"by_letter": _stringify(by_letter), "by_stage": _stringify(by_stage),
            "by_letter_stage": _stringify(by_letter_stage),
            "total_reserved_max_usd": str(sum((r["reserved_max_usd"] for r in by_letter.values()), Decimal(0))),
            "total_actual_usd": str(sum((r["actual_usd"] for r in by_letter.values()), Decimal(0))),
            "blocking_request_ids": state.blocking()}


def _stringify(table):
    return {key: {k: (str(v) if isinstance(v, Decimal) else v) for k, v in row.items()}
            for key, row in sorted(table.items())}


def _extraction(files):
    sessions = [s for f in files for s in (f.get("sessions") or [])]
    requests = [len(s.get("requests") or []) for s in sessions]
    fingerprints = [f.get("fingerprints") for f in files if isinstance(f.get("fingerprints"), dict)]
    configs = sorted({f.get("effective_config_sha256") for f in files if f.get("effective_config_sha256")})
    return {"histories": len(files), "sessions": len(sessions),
            "zero_facts_sessions": sum(bool(s.get("zero_facts")) for s in sessions),
            "capped_sessions": sum(bool(s.get("capped")) for s in sessions),
            "discarded_facts_total": sum(int(s.get("discarded_facts") or 0) for s in sessions),
            "requests_total": sum(requests),
            "requests_per_session": {"min": min(requests) if requests else None,
                                     "median": statistics.median(requests) if requests else None,
                                     "max": max(requests) if requests else None},
            "halted": any(f.get("halted") for f in files),
            "fingerprints": fingerprints[0] if fingerprints else None,
            "fingerprints_consistent": all(fp == fingerprints[0] for fp in fingerprints),
            "effective_config_sha256": configs}


def _capped_questions(files, contexts, holdout):
    """Questions whose history has at least one ``extraction_capped`` session (from the holdout when given)."""
    capped = {f.get("history_id") for f in files if any(s.get("capped") for s in (f.get("sessions") or []))}
    if holdout is not None:
        histories = {q: row.get("history_id") for q, row in holdout.questions.items()}
    else:
        histories = {q: c.get("history_id") for q, c in contexts.items()}
    return sorted(q for q, hid in histories.items() if hid in capped)


def _input_tokens(probes):
    by_stage = {}
    for probe in probes:
        by_stage[probe.get("stage")] = by_stage.get(probe.get("stage"), 0) + 1
    return {"probes": len(probes), "input_tokens_total": sum(int(p.get("input_tokens") or 0) for p in probes),
            "by_stage": dict(sorted(by_stage.items(), key=lambda kv: str(kv[0])))}


def build_report(campaign, journal_state, directory, holdout=None, clock=None):
    directory = Path(directory)
    contexts = {c["question_id"]: c for c in _read_dir(directory, "contexts") if "question_id" in c}
    arms = {(a["question_id"], a["arm"]): a for a in _read_dir(directory, "arms") if "arm" in a}
    verdicts = {(v["question_id"], v["arm"]): v for v in _read_dir(directory, "verdicts") if "arm" in v}
    per_arm, states = _arm_table(_expected(holdout, contexts, arms), contexts, arms, verdicts)
    pins = {alias: pin.get("model") for alias, pin in sorted(journal_state.pins.items())}
    if not pins:
        pins = {p.get("alias"): p.get("model") for p in _read_dir(directory, "pins") if "alias" in p}
    totals = campaign.ledger.totals()
    extraction = _read_dir(directory, "extraction")
    report = {"schema": 1, "campaign_id": campaign.campaign_id, "mode": campaign.mode,
              "bound_policy": campaign.policy, "limit_usd": str(campaign.limit),
              "sublimits_usd": {k: str(v) for k, v in campaign.sublimits.items()},
              "ledger": {k: (str(v) if isinstance(v, Decimal) else v) for k, v in totals.items()},
              "per_arm": per_arm, "states": states, "contexts": len(contexts),
              "zero_facts_questions": sorted(q for q, c in contexts.items() if c.get("zero_facts")),
              "capped_questions": _capped_questions(extraction, contexts, holdout),
              "money": _money_rows(journal_state), "extraction": _extraction(extraction),
              "zero_facts_events": len(_read_dir(directory, "events", skip=(".extraction_capped.json",))),
              "input_tokens": _input_tokens(_read_dir(directory, "counts")), "pins": pins}
    reviews = _read_dir(directory, "reviews")
    if reviews:  # only after --phase review: a campaign without reviews keeps the exact earlier report
        report["review"] = _review_summary(reviews)
    if clock is not None:
        report["generated_at"] = clock()
    return report


def _review_summary(reviews):
    """Counts over ``reviews/*.json`` (plan §8C); verdicts and scores above are never changed by a review."""
    def count(values):
        out = {}
        for value in values:
            out[str(value)] = out.get(str(value), 0) + 1
        return dict(sorted(out.items()))
    valid = [r["review"] for r in reviews if isinstance(r.get("review"), dict) and r["review"].get("valid")]
    return {"artifacts": len(reviews), "by_status": count(r.get("status") for r in reviews),
            "by_arm": count(r.get("arm") for r in reviews),
            "agrees_with_judge": count(v["agrees_with_judge"] for v in valid),
            "proposed_correction": count(v["proposed_correction"] for v in valid),
            "critical": count(bool(v.get("critical")) for v in valid),
            "critical_error_types": count(t for v in valid for t in v.get("critical_error_types") or []),
            "review_list_sha256": sorted({r.get("review_list_sha256") for r in reviews if r.get("review_list_sha256")})}


def write_report(campaign, report):
    """Create-only ``reports/report-<n>.json`` (n = one past the highest existing index)."""
    folder = Path(campaign.directory) / "reports"
    folder.mkdir(parents=True, exist_ok=True)
    existing = [int(p.stem.split("-")[-1]) for p in folder.glob("report-*.json") if p.stem.split("-")[-1].isdigit()]
    path = folder / f"report-{(max(existing) + 1) if existing else 1}.json"
    write_create_only(path, report)
    return path
