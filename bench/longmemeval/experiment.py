#!/usr/bin/env python3
"""Offline, frozen LongMemEval splits and intention-to-evaluate QA statistics.

No model calls, answer inspection, or tuning on evaluation questions. Metrics take
strict observations returned by validate.read_results (manifest-bound QA labels).
"""
import argparse
from collections import Counter, defaultdict
import hashlib
import json
import math
from pathlib import Path
import random

from validate import canonical_hash, counts, sha256
from qa_prompts import is_abstention


def history_hash(question):
    """Order-independent full-history identity; excludes gold answer annotations."""
    sessions = question.get("haystack_sessions")
    if not isinstance(sessions, list) or not sessions:
        raise ValueError("Each question requires nonempty haystack_sessions")
    hashes = []
    for session in sessions:
        if not isinstance(session, list) or not session:
            raise ValueError("History sessions must be nonempty turn lists")
        turns = []
        for turn in session:
            if not isinstance(turn, dict) or not isinstance(turn.get("content"), str):
                raise ValueError("History turns require text content")
            if not isinstance(turn.get("role"), str):
                raise ValueError("History turns require a role")
            turns.append([turn["role"], turn["content"]])
        hashes.append(canonical_hash(turns))
    return canonical_hash(sorted(hashes))


def question_index(questions):
    result = {}
    for question in questions:
        qid, category = question.get("question_id"), question.get("question_type")
        if not isinstance(qid, str) or not qid or qid in result:
            raise ValueError("Question IDs must be unique nonempty strings")
        if not isinstance(category, str) or not category:
            raise ValueError("Question categories must be nonempty strings")
        result[qid] = {"question_id": qid, "question_type": category,
                       "abstention": is_abstention(qid),
                       "history_sha256": history_hash(question)}
    return result


def _balanced_ids(index, size, seed):
    if type(size) is not int or not 0 < size <= len(index):
        raise ValueError("Split size must be positive and no larger than candidate pool")
    groups = defaultdict(list)
    for qid, row in sorted(index.items()):
        groups[row["history_sha256"]].append(qid)
    rank = lambda digest: hashlib.sha256(f"{seed}:{digest}".encode()).hexdigest()
    available = sorted(groups, key=rank)
    selected, category_counts = [], Counter()
    categories = sorted({row["question_type"] for row in index.values()})

    def take(digest):
        selected.extend(groups[digest])
        category_counts.update(index[qid]["question_type"] for qid in groups[digest])
        available.remove(digest)

    # Include abstention in every category that supplies it, before balanced fill.
    for category in categories:
        candidates = [digest for digest in available if any(
            index[qid]["question_type"] == category and index[qid]["abstention"]
            for qid in groups[digest])]
        candidates = [digest for digest in candidates if len(selected) + len(groups[digest]) <= size]
        if candidates:
            take(candidates[0])
    while len(selected) < size:
        choice = None
        for category in sorted(categories, key=lambda name: (category_counts[name], name)):
            choice = next((digest for digest in available
                           if len(selected) + len(groups[digest]) <= size
                           and any(index[qid]["question_type"] == category
                                   for qid in groups[digest])), None)
            if choice is not None:
                break
        if choice is None:
            raise ValueError("Cannot achieve exact split size without splitting a history group")
        take(choice)
    return sorted(selected)


def make_splits(questions, dev_size=120, pilot_size=18, seed=42):
    if type(seed) is not int:
        raise ValueError("Seed must be an integer")
    index = question_index(questions)
    if dev_size >= len(index):
        raise ValueError("Development split must leave a held-out evaluation split")
    dev = _balanced_ids(index, dev_size, seed)
    pilot = _balanced_ids({qid: index[qid] for qid in dev}, pilot_size, seed)
    evaluation = sorted(set(index) - set(dev))
    return {"seed": seed, "history_hash_policy": "sha256-sorted-session-role-content-v1",
            "dev": [index[qid] for qid in dev],
            "pilot": [index[qid] for qid in pilot],
            "eval": [index[qid] for qid in evaluation],
            "policy": "Tune only on dev; pilot is a dev subset. Freeze configuration before eval."}


def corpus_stats(questions):
    sessions = sum(len(question["haystack_sessions"]) for question in questions)
    turns = [turn for question in questions for session in question["haystack_sessions"]
             for turn in session]
    characters = sum(len(turn["content"]) for turn in turns)
    return {"questions": len(questions), "sessions": sessions, "turns": len(turns),
            "content_characters": characters,
            "rough_content_tokens_chars_div4": characters / 4,
            "token_estimate_warning": "Planning estimate only; excludes framing, retries and outputs. Not tokenizer or billing counts.",
            "categories": dict(sorted(Counter(q["question_type"] for q in questions).items())),
            "abstentions": sum(is_abstention(q["question_id"]) for q in questions),
            "raw_local_ingest_requests": len(turns)}


def build_manifest(dataset, dev_size=120, pilot_size=18, seed=42):
    questions = json.loads(Path(dataset).read_text())
    if not isinstance(questions, list):
        raise ValueError("Dataset must be a JSON question list")
    splits = make_splits(questions, dev_size, pilot_size, seed)
    by_id = {question["question_id"]: question for question in questions}
    stats = {name: corpus_stats([by_id[row["question_id"]] for row in splits[name]])
             for name in ("dev", "pilot", "eval")}
    return {"schema_version": 1, "dataset_sha256": sha256(dataset), **splits,
            "corpus": corpus_stats(questions), "split_stats": stats}


def wilson_interval(correct, expected, z=1.959963984540054):
    if type(correct) is not int or type(expected) is not int or not 0 <= correct <= expected:
        raise ValueError("Wilson counts must be integers with 0 <= correct <= expected")
    if expected == 0:
        return None
    p, z2 = correct / expected, z * z
    denominator = 1 + z2 / expected
    center = (p + z2 / (2 * expected)) / denominator
    radius = z * math.sqrt(p * (1 - p) / expected + z2 / (4 * expected ** 2)) / denominator
    return [max(0.0, center - radius), min(1.0, center + radius)]


def _validate_observations(expected, observations):
    ids = [row["question_id"] for row in expected]
    if len(ids) != len(set(ids)) or set(observations) - set(ids):
        raise ValueError("Duplicate expected IDs or observations outside expected split")
    if any(value is not None and type(value) is not bool for value in observations.values()):
        raise ValueError("Observations must be strict boolean QA labels or None for failure")
    return ids


def _slice_metrics(ids, observations):
    result = counts(ids, observations)
    result["wilson_95"] = wilson_interval(result["correct"], result["expected"])
    result["small_sample"] = len(ids) < 30
    return result


def qa_metrics(expected, observations):
    """All expected questions are denominator; missing and failed count incorrect."""
    ids = _validate_observations(expected, observations)
    categories = sorted({row["question_type"] for row in expected})
    return {"summary": _slice_metrics(ids, observations),
            "categories": {category: _slice_metrics(
                [row["question_id"] for row in expected if row["question_type"] == category],
                observations) for category in categories},
            "abstentions": _slice_metrics(
                [row["question_id"] for row in expected if row["abstention"]], observations),
            "uncertainty_note": "Wilson 95% intervals assume independent questions; shared-history dependence is not accounted for. Small pilots are descriptive, not evidence of improvement."}


def paired_metrics(expected, baseline, candidate, seed=42, bootstrap_samples=2000):
    ids = _validate_observations(expected, baseline)
    _validate_observations(expected, candidate)
    if type(bootstrap_samples) is not int or bootstrap_samples < 100:
        raise ValueError("At least 100 bootstrap samples required")
    deltas = {qid: int(candidate.get(qid) is True) - int(baseline.get(qid) is True) for qid in ids}
    groups = defaultdict(list)
    for row in expected:
        groups[row["history_sha256"]].append(deltas[row["question_id"]])
    blocks, samples = [groups[key] for key in sorted(groups)], []
    rng = random.Random(seed)
    if len(blocks) >= 2:
        for _ in range(bootstrap_samples):
            drawn = [value for _ in blocks for value in rng.choice(blocks)]
            samples.append(sum(drawn) / len(drawn))
        samples.sort()
    interval = [samples[int((len(samples) - 1) * tail)] for tail in (0.025, 0.975)] if samples else None
    return {"expected": len(ids), "improved": sum(value == 1 for value in deltas.values()),
            "regressed": sum(value == -1 for value in deltas.values()),
            "unchanged": sum(value == 0 for value in deltas.values()),
            "accuracy_delta": sum(deltas.values()) / len(ids) if ids else None,
            "history_groups": len(blocks), "paired_history_bootstrap_95": interval,
            "bootstrap_seed": seed, "bootstrap_samples": bootstrap_samples,
            "small_sample": len(blocks) < 30,
            "uncertainty_note": "Percentile bootstrap resamples entire exact-history groups, includes failures/missing as incorrect; small/degenerate samples can produce misleadingly narrow intervals. No efficacy claim from pilot."}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--dataset", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--dev-size", type=int, default=120)
    parser.add_argument("--pilot-size", type=int, default=18)
    parser.add_argument("--seed", type=int, default=42)
    args = parser.parse_args()
    if args.output.resolve() == args.dataset.resolve():
        parser.error("Output cannot overwrite dataset")
    manifest = build_manifest(args.dataset, args.dev_size, args.pilot_size, args.seed)
    # Freeze by creating exclusively. Choose a new output path for a new experiment.
    with args.output.open("x") as output:
        json.dump(manifest, output, indent=2, sort_keys=True)
        output.write("\n")
    print(json.dumps({"manifest_sha256": canonical_hash(manifest),
                      "split_sizes": {name: len(manifest[name]) for name in ("dev", "pilot", "eval")},
                      "pilot_stats": manifest["split_stats"]["pilot"]}, sort_keys=True))


if __name__ == "__main__":
    main()
