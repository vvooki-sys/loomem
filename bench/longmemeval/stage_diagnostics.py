#!/usr/bin/env python3
"""Offline retrieval stage diagnostics for LongMemEval traces; standard library only.

Given a retrieval trace JSONL (rows written by `runner.py`, with evidence stages
`ranked_candidates`, `after_soft_delete`, `after_filters_and_dedup`, `response`),
a "relevant IDs" JSON object (`{question_id: [record_id, ...]}`, supplied
externally, e.g. from synthetic cases known by construction; never derived from
gold answers) and an optional packed-context JSON (`{question_id:
{"selected_ids": [...], "dropped_ids": [...]}}`, the fields of a
`pack_context_v2`/`pack_context` output), it reports per question and
aggregated where each relevant ID was present, its rank per stage, how many
records each stage dropped and, when rows carry an optional `rejections` list
of `{chunk_id, stage, reason}` (either top-level or inside `evidence`), a
histogram of rejection reasons. It sends no requests and reads no text: only
IDs and counts. It does not score answers.
"""
import argparse
import json
from pathlib import Path
import statistics
import sys

from validate import decode, fail

ENGINE_STAGES = ("ranked_candidates", "after_soft_delete", "after_filters_and_dedup", "response")
PACKED_STAGE = "packed_context"
STAGES = ENGINE_STAGES + (PACKED_STAGE,)


def stage_ids(row, packed=None):
    """Ordered IDs per stage; a stage that was not observed maps to None."""
    ids = {stage: None for stage in STAGES}
    evidence = row.get("evidence") if isinstance(row, dict) else None
    if isinstance(evidence, dict) and evidence.get("coverage") != "unavailable":
        for stage in evidence.get("stages", []):
            name, chunk_ids = stage.get("stage"), stage.get("chunk_ids")
            if name not in ENGINE_STAGES or not isinstance(chunk_ids, list):
                raise ValueError("Malformed evidence stage")
            if any(not isinstance(item, str) for item in chunk_ids):
                raise ValueError("Stage chunk IDs must be strings")
            ids[name] = list(chunk_ids)
    if packed is not None:
        selected = packed.get("selected_ids")
        if not isinstance(selected, list) or any(not isinstance(item, str) for item in selected):
            raise ValueError("Packed context must list selected_ids")
        ids[PACKED_STAGE] = list(selected)
    return ids


def rejections(row):
    """Optional, forward-compatible `rejections` list; malformed entries fail."""
    found = row.get("rejections")
    if found is None and isinstance(row.get("evidence"), dict):
        found = row["evidence"].get("rejections")
    if found is None:
        return []
    if not isinstance(found, list):
        raise ValueError("rejections must be a list")
    checked = []
    for item in found:
        if (not isinstance(item, dict) or set(item) != {"chunk_id", "stage", "reason"}
                or not all(isinstance(item[key], str) for key in item)):
            raise ValueError("Each rejection needs string chunk_id, stage and reason")
        checked.append(dict(item))
    return checked


def _rank(ids, record_id):
    if ids is None:
        return None
    try:
        return ids.index(record_id) + 1
    except ValueError:
        return None


def _dropped(ids, packed):
    dropped = {}
    for previous, stage in zip(ENGINE_STAGES, ENGINE_STAGES[1:]):
        before, after = ids[previous], ids[stage]
        dropped[stage] = None if before is None or after is None else len(before) - len(after)
    if packed is None:
        dropped[PACKED_STAGE] = None
    else:
        dropped_ids = packed.get("dropped_ids")
        if not isinstance(dropped_ids, list):
            raise ValueError("Packed context must list dropped_ids")
        dropped[PACKED_STAGE] = len(dropped_ids)
    return dropped


def _relevant_entry(ids, record_id, top_k):
    rank = {stage: _rank(ids[stage], record_id) for stage in STAGES}
    presence = {stage: None if ids[stage] is None else rank[stage] is not None for stage in STAGES}
    first_absent = None
    for stage in STAGES:
        if presence[stage] is False:
            first_absent = stage
            break
    pool_rank = rank["ranked_candidates"]
    beyond = None if pool_rank is None or top_k is None else pool_rank > top_k
    return {"presence": presence, "rank": rank, "first_absent": first_absent,
            "pool_rank_beyond_top_k": beyond}


def question_diagnostics(row, relevant_ids, packed=None):
    """Pure per-question report from one trace row and its external relevant IDs."""
    if not isinstance(relevant_ids, list) or any(not isinstance(item, str) for item in relevant_ids):
        raise ValueError("Relevant IDs must be a list of strings")
    if len(set(relevant_ids)) != len(relevant_ids):
        raise ValueError("Relevant IDs must be unique")
    ids = stage_ids(row, packed)
    top_k = None if ids["response"] is None else len(ids["response"])
    relevant = {record_id: _relevant_entry(ids, record_id, top_k) for record_id in relevant_ids}
    present, all_present, first_rank = {}, {}, {}
    for stage in STAGES:
        if ids[stage] is None:
            present[stage] = all_present[stage] = first_rank[stage] = None
            continue
        ranks = [entry["rank"][stage] for entry in relevant.values() if entry["rank"][stage] is not None]
        present[stage] = len(ranks)
        all_present[stage] = (len(ranks) == len(relevant_ids)) if relevant_ids else None
        first_rank[stage] = min(ranks) if ranks else None
    rejected = rejections(row)
    histogram = {}
    for item in rejected:
        histogram[item["reason"]] = histogram.get(item["reason"], 0) + 1
    return {"question_id": row.get("question_id"), "status": row.get("status"),
            "top_k": top_k,
            "stage_counts": {stage: None if ids[stage] is None else len(ids[stage]) for stage in STAGES},
            "dropped_per_stage": _dropped(ids, packed),
            "relevant_ids": list(relevant_ids), "relevant": relevant,
            "relevant_present": present, "all_relevant_present": all_present,
            "first_relevant_rank": first_rank,
            "rejections_by_reason": dict(sorted(histogram.items())),
            "relevant_rejections": [item for item in rejected if item["chunk_id"] in relevant]}


def _distribution(values):
    if not values:
        return {"n": 0, "min": None, "median": None, "max": None}
    return {"n": len(values), "min": min(values), "median": statistics.median(values),
            "max": max(values)}


def aggregate(questions):
    """Aggregate per-question reports; unmeasured stages never count as absent."""
    total = {"questions": len(questions),
             "questions_with_relevant": sum(1 for q in questions if q["relevant_ids"]),
             "relevant_ids_total": sum(len(q["relevant_ids"]) for q in questions),
             "relevant_present": {}, "questions_all_relevant_present": {},
             "first_relevant_rank": {}, "dropped_per_stage": {},
             "first_absent": {stage: 0 for stage in STAGES}, "never_absent": 0,
             "pool_rank_beyond_top_k": 0, "rejections_by_reason": {}}
    for stage in STAGES:
        measured = [q for q in questions if q["relevant_present"][stage] is not None]
        total["relevant_present"][stage] = {
            "present": sum(q["relevant_present"][stage] for q in measured),
            "of": sum(len(q["relevant_ids"]) for q in measured)}
        judged = [q for q in measured if q["all_relevant_present"][stage] is not None]
        total["questions_all_relevant_present"][stage] = {
            "count": sum(1 for q in judged if q["all_relevant_present"][stage]),
            "of": len(judged)}
        total["first_relevant_rank"][stage] = _distribution(
            [q["first_relevant_rank"][stage] for q in measured if q["first_relevant_rank"][stage] is not None])
        dropped = [q["dropped_per_stage"][stage] for q in questions
                   if stage != "ranked_candidates" and q["dropped_per_stage"][stage] is not None]
        total["dropped_per_stage"][stage] = sum(dropped) if dropped else None
    for question in questions:
        for entry in question["relevant"].values():
            if entry["first_absent"] is None:
                total["never_absent"] += 1
            else:
                total["first_absent"][entry["first_absent"]] += 1
            if entry["pool_rank_beyond_top_k"]:
                total["pool_rank_beyond_top_k"] += 1
        for reason, count in question["rejections_by_reason"].items():
            total["rejections_by_reason"][reason] = total["rejections_by_reason"].get(reason, 0) + count
    total["rejections_by_reason"] = dict(sorted(total["rejections_by_reason"].items()))
    return total


def diagnose(rows, relevant, packed=None):
    """Full report for trace rows, relevant IDs and optional packed contexts."""
    if not isinstance(relevant, dict):
        raise ValueError("Relevant IDs must map question IDs to ID lists")
    packed = {} if packed is None else packed
    questions, seen = [], set()
    for row in rows:
        question_id = row.get("question_id") if isinstance(row, dict) else None
        if not isinstance(question_id, str) or not question_id or question_id in seen:
            raise ValueError("Trace rows need unique string question IDs")
        seen.add(question_id)
        questions.append(question_diagnostics(row, relevant.get(question_id, []),
                                              packed.get(question_id)))
    unknown = sorted(set(relevant) - seen)
    return {"schema_version": 1, "stages": list(STAGES), "questions": questions,
            "aggregate": aggregate(questions), "relevant_without_trace": unknown}


def _cell(question, stage):
    present = question["relevant_present"][stage]
    if present is None:
        return "n/a"
    text = f"{present}/{len(question['relevant_ids'])}"
    rank = question["first_relevant_rank"][stage]
    return text if rank is None else f"{text} (r{rank})"


def markdown_report(report):
    """Short Markdown tables: one row per question, then per-stage aggregates."""
    lines = ["| question | relevant | pool | soft-delete | filters/dedup | response | packed | first absent |",
             "|---|---|---|---|---|---|---|---|"]
    for question in report["questions"]:
        absent = sorted({entry["first_absent"] for entry in question["relevant"].values()
                         if entry["first_absent"]}, key=STAGES.index)
        lines.append("| " + " | ".join([
            question["question_id"], str(len(question["relevant_ids"])),
            *(_cell(question, stage) for stage in STAGES),
            ", ".join(absent) or "-"]) + " |")
    total = report["aggregate"]
    lines += ["", "| stage | relevant present | questions fully covered | first rank (min/median/max) | dropped |",
              "|---|---|---|---|---|"]
    for stage in STAGES:
        present = total["relevant_present"][stage]
        covered = total["questions_all_relevant_present"][stage]
        rank = total["first_relevant_rank"][stage]
        rank_text = "-" if rank["n"] == 0 else f"{rank['min']}/{rank['median']}/{rank['max']}"
        dropped = total["dropped_per_stage"][stage]
        lines.append(f"| {stage} | {present['present']}/{present['of']} | "
                     f"{covered['count']}/{covered['of']} | {rank_text} | "
                     f"{'-' if dropped is None else dropped} |")
    lines += ["", f"Relevant IDs in the pool beyond top-k: {total['pool_rank_beyond_top_k']}; "
                  f"never absent: {total['never_absent']}."]
    if total["rejections_by_reason"]:
        lines += ["", "| rejection reason | count |", "|---|---|"]
        lines += [f"| {reason} | {count} |" for reason, count in total["rejections_by_reason"].items()]
    return "\n".join(lines) + "\n"


def load_rows(path):
    rows = []
    for number, line in enumerate(Path(path).read_text().splitlines(), 1):
        if not line.strip():
            fail(f"Blank line {number} in {path}")
        rows.append(decode(line, f"{path}:{number}"))
    return rows


def load_keyed(path):
    """A JSON object keyed by question ID, or JSONL rows carrying question_id."""
    if str(path).endswith(".jsonl"):
        keyed = {}
        for row in load_rows(path):
            if not isinstance(row, dict) or not isinstance(row.get("question_id"), str):
                fail("JSONL rows must carry question_id")
            if row["question_id"] in keyed:
                fail("Duplicate question_id")
            keyed[row["question_id"]] = row
        return keyed
    value = decode(Path(path).read_text(), str(path))
    if not isinstance(value, dict):
        fail(f"{path} must be a JSON object keyed by question ID")
    return value


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--trace", required=True, type=Path, help="Retrieval trace JSONL")
    parser.add_argument("--relevant", required=True, type=Path,
                        help="JSON object {question_id: [relevant record IDs]}")
    parser.add_argument("--packed", type=Path, help="Packed contexts keyed by question ID")
    parser.add_argument("--output", type=Path, help="Write the JSON report here")
    parser.add_argument("--markdown", type=Path, help="Write the Markdown tables here")
    parser.add_argument("--format", choices=("json", "markdown"), default="json",
                        help="What to print on stdout")
    args = parser.parse_args(argv)
    try:
        report = diagnose(load_rows(args.trace), load_keyed(args.relevant),
                          load_keyed(args.packed) if args.packed else None)
    except ValueError as error:
        print(f"invalid: {error}", file=sys.stderr)
        return 2
    text = json.dumps(report, ensure_ascii=False, indent=2, allow_nan=False) + "\n"
    table = markdown_report(report)
    if args.output:
        args.output.write_text(text)
    if args.markdown:
        args.markdown.write_text(table)
    sys.stdout.write(text if args.format == "json" else table)
    return 0


if __name__ == "__main__":
    sys.exit(main())
