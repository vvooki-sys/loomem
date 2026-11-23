"""Predicted maximum of a campaign: calls x reservation per letter/stage (plan §5, dry-run table).

Pure arithmetic over the sheet, the holdout and the campaign profiles; it is
what preflight check 5 compares with the sublimits and what ``dry-run`` prints.
Per question: F = diagnostic + answer + judge, S = diagnostic + audit +
selection + answer + judge, A (always-originals ids only) = diagnostic + answer
+ judge; R = one control call; E per session = ``predicted_chunks`` x
``E_CALLS_PER_CHUNK`` requests, each bounded by ``extraction_bound(session
bytes)`` with O = ``E_CHUNK_OUT``. Both policies are priced; a policy the
sheet cannot price yields ``"unpriced"`` instead of a number.
"""
from decimal import Decimal

from paid_call_journal import LETTERS
from paid_price_sheet import POLICIES, PriceUnknown
from runner_local_e1 import render_transcript as _render_turns

ARM_STAGES = {"F": ("diagnostic", "answer", "judge"),
              "S": ("diagnostic", "audit", "selection", "answer", "judge"),
              "A": ("diagnostic", "answer", "judge")}
ARM_OF_LETTER = {"F": "facts_retry", "S": "selective_originals", "A": "always_originals"}


def render_transcript(session):
    return _render_turns(session["turns"])


def predicted_chunks(text, chunk_chars):
    """Port of chunking.rs ``line_aligned_chunks`` counting (chars, split_inclusive('\\n'))."""
    try:
        from paid_engine import predicted_chunks as engine_count
    except ImportError:
        engine_count = None
    if engine_count is not None:
        return engine_count(text, chunk_chars)
    chunks, current = 0, 0
    for line in text.splitlines(keepends=True):
        length = len(line)
        if current > 0 and current + length > chunk_chars:
            chunks += 1
            current = 0
        current += length
    return chunks + 1 if (current > 0 or chunks == 0) else chunks


def selected_histories(holdout, limit_histories=None):
    """Histories in order of first appearance in the manifest's question order."""
    order = []
    for qid in holdout.question_order:
        hid = holdout.questions[qid]["history_id"]
        if hid not in order:
            order.append(hid)
    for hid in sorted(holdout.histories):
        if hid not in order:
            order.append(hid)
    if limit_histories is not None:
        order = order[:limit_histories]
    return order


def selected_questions(holdout, histories):
    chosen = set(histories)
    return [qid for qid in holdout.question_order if holdout.questions[qid]["history_id"] in chosen]


def _usd(sheet, bound):
    out = {}
    for policy in POLICIES:
        try:
            out[policy] = sheet.max_usd(bound, policy)
        except PriceUnknown:
            out[policy] = None
    return out


def _times(per_call, calls):
    return {p: (None if v is None else v * calls) for p, v in per_call.items()}


def _add(total, part):
    for policy in POLICIES:
        if total[policy] is None or part[policy] is None:
            total[policy] = None
        else:
            total[policy] += part[policy]


def _text(value):
    return "unpriced" if value is None else str(value)


def _stage_rows(sheet, holdout, questions, always):
    rows = []
    sources_max = holdout.sources_bytes_max()
    for letter, stages in ARM_STAGES.items():
        calls = len(always) if letter == "A" else len(questions)
        for stage in stages:
            bound = sheet.stage_profile(stage, sources_max, True)
            rows.append({"letter": letter, "stage": stage, "model": bound.model, "calls": calls,
                         "r_tokens": bound.r_tokens, "o_tokens": bound.o_tokens,
                         "tokens_max": (bound.r_tokens + bound.o_tokens) * calls,
                         "usd": _times(_usd(sheet, bound), calls)})
    control = sheet.control_bound()
    rows.append({"letter": "K", "stage": control.stage, "model": control.model, "calls": 1,
                 "r_tokens": control.r_tokens, "o_tokens": control.o_tokens,
                 "tokens_max": control.r_tokens + control.o_tokens, "usd": _usd(sheet, control)})
    return rows


def _session_rows(sheet, holdout, histories, token_counter):
    rows = []
    chunk_chars = sheet.extraction_chunk_chars()
    per_chunk = int(sheet.module.E_CALLS_PER_CHUNK)
    for hid in histories:
        for session in holdout.histories[hid]["sessions"]:
            text = render_transcript(session)
            size = len(text.encode("utf-8"))
            chunks = predicted_chunks(text, chunk_chars)
            bound = sheet.extraction_bound(size)
            requests = chunks * per_chunk
            rows.append({"letter": "E", "history_id": hid, "session_id": session["session_id"],
                         "bytes": size, "predicted_chunks": chunks, "max_requests": requests,
                         "r_tokens": bound.r_tokens, "o_tokens": bound.o_tokens,
                         "tokens_max": (bound.r_tokens + bound.o_tokens) * requests,
                         "o200k_tokens": token_counter(text) if token_counter else None,
                         "usd": _times(_usd(sheet, bound), requests)})
    return rows


def predicted_maximum(sheet, campaign_spec, holdout, *, limit_histories=None, token_counter=None):
    """The dry-run table as a JSON-ready dict (Decimals rendered as strings)."""
    histories = selected_histories(holdout, limit_histories)
    questions = selected_questions(holdout, histories)
    always = [qid for qid in questions if qid in holdout.always_ids]
    stage_rows = _stage_rows(sheet, holdout, questions, always)
    session_rows = _session_rows(sheet, holdout, histories, token_counter)
    by_letter = {letter: {p: Decimal(0) for p in POLICIES} for letter in LETTERS}
    for row in stage_rows + session_rows:
        _add(by_letter[row["letter"]], row["usd"])
    total = {p: Decimal(0) for p in POLICIES}
    for letter in LETTERS:
        _add(total, by_letter[letter])
    limit = Decimal(str(campaign_spec["limit_usd"]))
    sublimits = {k: Decimal(str(v)) for k, v in campaign_spec["sublimits_usd"].items()}
    sub_sum = sum(sublimits.values(), Decimal(0))
    chosen = campaign_spec["bound_policy"]
    render = lambda row: {**row, "usd_max": {p: _text(v) for p, v in row["usd"].items()}, "usd": None}
    return {
        "policy_chosen": chosen,
        "histories": histories, "questions": len(questions), "always_ids": len(always),
        "rows": [{k: v for k, v in render(r).items() if k != "usd"} for r in stage_rows],
        "extraction_sessions": [{k: v for k, v in render(r).items() if k != "usd"} for r in session_rows],
        "by_letter": {letter: {p: _text(v) for p, v in by_letter[letter].items()} for letter in LETTERS},
        "total": {p: _text(v) for p, v in total.items()},
        "sublimits_usd": {k: str(v) for k, v in sublimits.items()},
        "sublimits_sum": str(sub_sum), "limit_usd": str(limit),
        "surplus_or_deficit": {p: _text(None if v is None else limit - v) for p, v in total.items()},
        "letters_over_sublimit": {p: sorted(letter for letter in LETTERS
                                            if by_letter[letter][p] is None or by_letter[letter][p] > sublimits[letter])
                                  for p in POLICIES},
    }


def format_table(maximum):
    """Plain-text rendering for the terminal (the JSON is the machine-readable form)."""
    chosen = maximum["policy_chosen"]
    lines = [f"PREDICTED MAXIMUM (policy {chosen} chosen; histories={len(maximum['histories'])} "
             f"questions={maximum['questions']} always={maximum['always_ids']})",
             f"{'L':<2} {'stage':<12} {'calls':>6} {'R':>8} {'O':>7} {'tokens_max':>12} {'USD p1':>14} {'USD p2':>14}"]
    for row in maximum["rows"]:
        lines.append(f"{row['letter']:<2} {row['stage']:<12} {row['calls']:>6} {row['r_tokens']:>8} "
                     f"{row['o_tokens']:>7} {row['tokens_max']:>12} {row['usd_max']['p1']:>14} {row['usd_max']['p2']:>14}")
    for row in maximum["extraction_sessions"]:
        label = f"{row['history_id']}/{row['session_id']}"[:12]
        lines.append(f"E  {label:<12} {row['max_requests']:>6} {row['r_tokens']:>8} {row['o_tokens']:>7} "
                     f"{row['tokens_max']:>12} {row['usd_max']['p1']:>14} {row['usd_max']['p2']:>14}"
                     f"  chunks={row['predicted_chunks']} bytes={row['bytes']} o200k={row['o200k_tokens']}")
    for letter, usd in maximum["by_letter"].items():
        lines.append(f"sum {letter}: p1={usd['p1']} p2={usd['p2']} sublimit={maximum['sublimits_usd'][letter]}")
    lines.append(f"total: p1={maximum['total']['p1']} p2={maximum['total']['p2']} "
                 f"sublimits_sum={maximum['sublimits_sum']} limit={maximum['limit_usd']} "
                 f"surplus_or_deficit[{chosen}]={maximum['surplus_or_deficit'][chosen]}")
    return "\n".join(lines)
