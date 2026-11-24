"""The four phases shared by ``run`` and ``dry-run``: extraction, retrieval, arms, judge (plan §3/§4).

A session whose extraction hit the per-chunk fact cap (engine ``partial``, facts discarded) is the
explicit state ``extraction_capped``, not a stop: its stored facts are used and its questions stay in
the denominator; ``events/<hid>.<sid>.extraction_capped.json`` and the report count it.

Every phase is resumable by construction: work whose artifact file exists is
skipped (``extraction/<hid>.json``, ``contexts/<qid>.json``,
``arms/<qid>.<arm>.json``, ``verdicts/<qid>.<arm>.json``) and identical paid
requests replay from the journal inside ``paid_completion``. One fresh engine
process and data directory per history; questions only read. The proxy is the
only path from the engine to the provider; a halted proxy stops the run.
Fail-closed stops (``RunStopped`` family, budget, sublimit, campaign) propagate.
"""
from dataclasses import dataclass
import json
from pathlib import Path
import sys
import time

from budget import BudgetError
from paid_call_journal import write_create_only
from paid_campaign import SublimitExceeded
from paid_maximum import predicted_chunks, render_transcript, selected_histories, selected_questions
from paid_openai_wire import RunStopped

ARMS = {"facts_retry": "F", "selective_originals": "S", "always_originals": "A"}


def _stderr(line):
    print(line, file=sys.stderr, flush=True)


@dataclass
class RunContext:
    campaign: object
    journal: object
    sheet: object
    prompts: object
    holdout: object
    transport: object
    engine_factory: object          # callable(proxy_url, proxy_token) -> engine (start/ingest_session/...)
    proxy: object                   # a started ProviderProxy (url/token/bind/unbind/halted/records)
    precount: object = None
    acknowledged: frozenset = frozenset()
    limit_histories: int | None = None
    emit: object = _stderr
    clock: object = time.time
    readiness_deadline_s: float = 60.0
    after_session: object = None    # optional callable(proxy) after each session (dry-run role registration)

    @property
    def directory(self):
        return Path(self.campaign.directory)

    def histories(self):
        return selected_histories(self.holdout, self.limit_histories)

    def questions(self, hid=None):
        qids = selected_questions(self.holdout, self.histories())
        return [q for q in qids if hid is None or self.holdout.questions[q]["history_id"] == hid]

    def arms_for(self, qid):
        return [arm for arm in ARMS if arm != "always_originals" or qid in self.holdout.always_ids]

    def set_count_context(self, **fields):
        if hasattr(self.precount, "set_context"):
            self.precount.set_context(**fields)

    def acknowledged_bindings(self):
        """(stage, history_id, question_id, arm) of every acknowledged uncertain request."""
        out = {}
        for rid in self.acknowledged:
            intent = self.journal.state.intents.get(rid)
            if intent is not None:
                b = intent["binding"]
                out[(b.get("stage"), b.get("history_id"), b.get("question_id"), b.get("arm"))] = rid
        return out


def _read(path):
    return json.loads(Path(path).read_text(encoding="utf-8"))


# -- E ---------------------------------------------------------------------
def _extraction_path(ctx, hid):
    return ctx.directory / "extraction" / f"{hid}.json"


def _acknowledged_extraction(ctx, hid):
    for (stage, history, _, _), rid in ctx.acknowledged_bindings().items():
        if stage == "extraction" and history == hid:
            return rid
    return None


def _ingest(ctx, engine, hid, session):
    sid, text = session["session_id"], render_transcript(session)
    cap = predicted_chunks(text, ctx.sheet.extraction_chunk_chars()) * int(ctx.sheet.module.E_CALLS_PER_CHUNK)
    bound = ctx.sheet.extraction_bound(len(text.encode("utf-8")))
    ctx.set_count_context(stage="extraction", arm=None, question_id=None, history_id=hid, session_id=sid,
                          r_tokens=bound.r_tokens)
    ctx.proxy.bind(hid, sid, cap)
    try:
        receipt = engine.ingest_session(session)
    except Exception as exc:
        records = ctx.proxy.unbind()
        if ctx.proxy.halted:
            raise RunStopped(f"proxy halted during {hid}/{sid}: {ctx.proxy.halt_reason}") from exc
        raise
    records = ctx.proxy.unbind()
    if ctx.proxy.halted:
        raise RunStopped(f"proxy halted during {hid}/{sid}: {ctx.proxy.halt_reason}")
    if ctx.after_session is not None:
        ctx.after_session(ctx.proxy)
    proof = engine.prove_readiness_v2(hid, receipt, session, ctx.readiness_deadline_s)
    engine.fetch_source(receipt.source_id)
    if receipt.zero_facts:
        # Keyed by history/session, not by the engine's source id: a history that is redone after an
        # interrupted run gets fresh source ids, and the event must be safe to repeat.
        event = ctx.directory / "events" / f"{hid}.{sid}.zero_facts.json"
        if not event.exists():
            write_create_only(event, {"history_id": hid, "session_id": sid, "source_id": receipt.source_id,
                                      "at": ctx.clock()})
    requests = [r["request_id"] for r in records if r.get("request_id")]
    if receipt.capped:
        # The product dropped facts above max_facts_per_chunk (engine status `partial`): not a stop and not
        # zero_facts. The session keeps its stored facts and its questions stay in every arm's denominator.
        event = ctx.directory / "events" / f"{hid}.{sid}.extraction_capped.json"
        if not event.exists():
            write_create_only(event, {"type": "extraction_capped", "history_id": hid, "session_id": sid,
                                      "source_id": receipt.source_id, "discarded_facts": receipt.discarded_facts,
                                      "requests": requests, "at": ctx.clock()})
    return {"session_id": sid, "source_id": receipt.source_id, "conversation_date": session["conversation_date"],
            "fragment_ids": list(receipt.fragment_ids), "fact_ids": list(receipt.fact_ids),
            "zero_facts": bool(receipt.zero_facts), "requests": requests,
            "records": records, "proof_sha256": _sha(proof), "predicted_max_requests": cap,
            "capped": bool(receipt.capped), "discarded_facts": receipt.discarded_facts}


def _sha(obj):
    from paid_call_journal import canonical_bytes, sha256_hex
    return sha256_hex(canonical_bytes(obj))


def _move_aside(ctx, work):
    if work.exists():
        stale = work.with_name(f"{work.name}.partial-{int(ctx.clock())}")
        work.rename(stale)
        ctx.emit(f"E {work.name}: kept partial engine dir as {stale.name}; starting fresh (replays are free)")


def phase_extraction(ctx):
    for hid in ctx.histories():
        if _extraction_path(ctx, hid).exists():
            continue
        rid = _acknowledged_extraction(ctx, hid)
        if rid is not None:
            write_create_only(_extraction_path(ctx, hid), {"history_id": hid, "sessions": [], "halted": True,
                                                           "error": f"acknowledged_uncertain:{rid}"})
            ctx.emit(f"E {hid}: skipped (acknowledged uncertain extraction request {rid[:12]})")
            continue
        work = ctx.directory / "engine" / hid
        _move_aside(ctx, work)
        engine = ctx.engine_factory(ctx.proxy.url, ctx.proxy.token)
        try:  # start() inside the cleanup block: a failed start must not leave a live child
            engine.start(work, reuse=False)
            sessions = [_ingest(ctx, engine, hid, s) for s in ctx.holdout.histories[hid]["sessions"]]
            write_create_only(_extraction_path(ctx, hid), {
                "history_id": hid, "sessions": sessions, "halted": False,
                "fingerprints": engine.fingerprints(), "effective_config_sha256": engine.effective_config_sha256,
                "system_sha256": ctx.proxy.system_sha256})
        finally:
            engine.close()
        _stop_if_halted(ctx, hid)
        ctx.emit(f"E {hid}: {len(sessions)} session(s), {sum(len(s['requests']) for s in sessions)} request(s)")


def _stop_if_halted(ctx, hid):
    """A halted proxy after an engine process ended is a stop, even when the engine reported success."""
    if ctx.proxy.halted:
        raise RunStopped(f"proxy halted while working on {hid}: {ctx.proxy.halt_reason}")


# -- retrieval ---------------------------------------------------------------
def _context_path(ctx, qid):
    return ctx.directory / "contexts" / f"{qid}.json"


def _error_path(ctx, qid):
    return ctx.directory / "events" / f"{qid}.context_error.json"


def _context_error(ctx, qid, hid, error):
    write_create_only(_error_path(ctx, qid), {"question_id": qid, "history_id": hid, "error": error, "at": ctx.clock()})
    ctx.emit(f"retrieval {qid}: technical error ({error[:80]})")


def _retrieve(ctx, engine, extraction, hid, pending):
    from paid_context import ContextError, build_context
    by_sid = {s["session_id"]: s for s in ctx.holdout.histories[hid]["sessions"]}
    sources = {s["source_id"]: render_transcript(by_sid[s["session_id"]]) for s in extraction["sessions"]}
    zero = all(s["zero_facts"] for s in extraction["sessions"])
    for qid in pending:
        view = ctx.holdout.execution_view(qid)
        search_json = engine.search(view["question"], view["question_date"])
        try:
            context = build_context(view, search_json, sources, ctx.sheet.caps(), zero)
        except ContextError as exc:
            _context_error(ctx, qid, hid, f"ContextError: {exc}")
            continue
        write_create_only(_context_path(ctx, qid), context)


def phase_retrieval(ctx):
    for hid in ctx.histories():
        path = _extraction_path(ctx, hid)
        if not path.exists():
            raise RunStopped(f"extraction of {hid} is missing: run --phase extraction first")
        extraction = _read(path)
        pending = [q for q in ctx.questions(hid) if not _context_path(ctx, q).exists() and not _error_path(ctx, q).exists()]
        if not pending:
            continue
        if extraction.get("halted"):
            for qid in pending:
                _context_error(ctx, qid, hid, f"extraction_halted: {extraction.get('error')}")
            continue
        engine = ctx.engine_factory(ctx.proxy.url, ctx.proxy.token)
        try:
            engine.start(ctx.directory / "engine" / hid, reuse=True)
            _retrieve(ctx, engine, extraction, hid, pending)
        finally:
            engine.close()
        _stop_if_halted(ctx, hid)
        ctx.emit(f"retrieval {hid}: {len(pending)} question(s)")


# -- arms ----------------------------------------------------------------------
def _deps(ctx):
    from paid_arms import ArmDeps
    return ArmDeps(ctx.campaign, ctx.journal, ctx.sheet, ctx.prompts, ctx.transport,
                   ctx.holdout.sources_bytes_max(), ctx.campaign.spec["holdout"]["manifest_sha256"],
                   frozenset(ctx.acknowledged), ctx.precount, ctx.clock)


def _arm_reserved_before(ctx, qid, arm):
    return any(i["binding"].get("question_id") == qid and i["binding"].get("arm") == arm
               and i["binding"].get("stage") != "judge" for i in ctx.journal.state.intents.values())


def _run_one_arm(ctx, deps, qid, arm, acked):
    from paid_arms import ArmOutcome, run_arm, write_arm_outcome
    hid = ctx.holdout.questions[qid]["history_id"]
    if _error_path(ctx, qid).exists():
        error = _read(_error_path(ctx, qid))["error"]
        return write_arm_outcome(ctx.directory, ArmOutcome(qid, arm, "technical_error", error=f"context: {error}"))
    if not _context_path(ctx, qid).exists():
        raise RunStopped(f"context of {qid} is missing: run --phase retrieval first")
    for (stage, _, question, arm_name), rid in acked.items():
        if question == qid and arm_name == arm and stage != "judge":
            return write_arm_outcome(ctx.directory, ArmOutcome(qid, arm, "technical_error",
                                                                error=f"acknowledged_uncertain:{rid}"))
    context = _read(_context_path(ctx, qid))
    ctx.set_count_context(stage=None, arm=arm, question_id=qid, history_id=hid, session_id=None, r_tokens=None)
    try:
        outcome = run_arm(arm, context, deps)
    except (SublimitExceeded, BudgetError) as exc:
        status = "technical_error" if _arm_reserved_before(ctx, qid, arm) else "not_started_budget"
        write_arm_outcome(ctx.directory, ArmOutcome(qid, arm, status, zero_facts=bool(context.get("zero_facts")),
                                                     error=f"{type(exc).__name__}: {exc}"))
        raise
    return write_arm_outcome(ctx.directory, outcome)


def phase_arms(ctx):
    from paid_arms import arm_path
    deps, acked, done = _deps(ctx), ctx.acknowledged_bindings(), 0
    for qid in ctx.questions():
        for arm in ctx.arms_for(qid):
            if arm_path(ctx.directory, qid, arm).exists():
                continue
            _run_one_arm(ctx, deps, qid, arm, acked)
            done += 1
    ctx.emit(f"arms: {done} arm run(s) this invocation")


# -- judge -------------------------------------------------------------------
def _minimal_context(ctx, qid):
    view = ctx.holdout.execution_view(qid)
    return {**view, "facts": [], "menu": [], "source_ids": [], "zero_facts": True, "context_sha256": "0" * 64}


def phase_judge(ctx):
    from paid_judge import judge_arm, read_verdict, write_verdict
    from paid_arms import read_arm_outcome
    deps, acked, done = _deps(ctx), ctx.acknowledged_bindings(), 0
    for qid in ctx.questions():
        hid = ctx.holdout.questions[qid]["history_id"]
        for arm in ctx.arms_for(qid):
            outcome = read_arm_outcome(ctx.directory, qid, arm)
            if outcome is None or read_verdict(ctx.directory, qid, arm) is not None:
                continue
            rid = acked.get(("judge", hid, qid, arm))
            if rid is not None:
                write_verdict(ctx.directory, {"question_id": qid, "arm": arm, "candidate": outcome.answer, "verdict": None,
                                              "score": "technical_error", "request_id": rid, "usage": None,
                                              "outcome": f"acknowledged_uncertain:{rid}"})
                continue
            context = _read(_context_path(ctx, qid)) if _context_path(ctx, qid).exists() else _minimal_context(ctx, qid)
            ctx.set_count_context(stage="judge", arm=arm, question_id=qid, history_id=hid, session_id=None, r_tokens=None)
            write_verdict(ctx.directory, judge_arm(deps, ctx.holdout, context, arm, outcome))
            done += 1
    ctx.emit(f"judge: {done} verdict(s) this invocation")


PHASES = {"extraction": phase_extraction, "retrieval": phase_retrieval, "arms": phase_arms, "judge": phase_judge}


def run_phases(ctx, phase="all"):
    if phase == "control":
        return []  # the control call K, sent by the caller before this, is the whole invocation
    names = list(PHASES) if phase == "all" else [phase]
    for name in names:
        PHASES[name](ctx)
    return names
