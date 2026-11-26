#!/usr/bin/env python3
"""Offline, deterministic sampler for the plan §8C review list (0 USD, no network, no provider).

The validation side draws the list; the runner only consumes it (``run --phase review
--review-list FILE --review-list-sha256 HEX``). Input: a ``verdicts/`` folder (a campaign's or its
evidence copy) and the pinned holdout (for categories). Output: a create-only JSON list whose
``pairs`` the runner reviews, and its SHA-256 on stdout.

Draw (plan §8C): ``--sample-size`` questions (100) stratified by category x judge agreement between
the arms (``agree`` when every judged arm of the question has the same score). Categories get a
largest-remainder proportional share (ties: a category with a ``disagree`` stratum first, then more
disagreements, then the name); inside a category the two strata split the same way, with at least 1
``disagree`` question whenever that stratum exists. ``random.Random(seed)`` draws from each stratum
in holdout order. Each drawn question contributes its ``--sample-arms`` pairs. Always added, beyond
the draw: every regression pair (F correct and S not: both F and S; F correct and A not: F and A)
and every non-correct answer in the critical categories (temporal_update, conflict_entity_assignment,
unanswerable). Only pairs with a judged candidate are reviewable; any other (e.g. ``technical_error``)
is listed under ``excluded_unreviewable``.
"""
import argparse
from fractions import Fraction
import hashlib
import json
import math
import os
from pathlib import Path
import random
import sys

from paid_arms import ARMS
from paid_holdout import verify_holdout
from paid_review import REVIEWABLE_SCORES

CRITICAL_CATEGORIES = ("temporal_update", "conflict_entity_assignment", "unanswerable")
STRATA = ("agree", "disagree")
F, S, A = "facts_retry", "selective_originals", "always_originals"


class SampleError(ValueError):
    """The inputs cannot produce the requested sample."""


def load_verdicts(folder):
    out = {}
    for path in sorted(Path(folder).glob("*.json")):
        if path.name.startswith("."):
            continue
        record = json.loads(path.read_text(encoding="utf-8"))
        key = (record.get("question_id"), record.get("arm"))
        if key[1] not in ARMS or key in out:
            raise SampleError(f"{path.name}: unknown arm or duplicate verdict")
        out[key] = record
    if not out:
        raise SampleError(f"no verdicts in {folder}")
    return out


def reviewable(record):
    return isinstance(record.get("candidate"), str) and record.get("score") in REVIEWABLE_SCORES


def strata_of(verdicts, order, categories):
    """{category: {"agree": [qid...], "disagree": [qid...]}} in holdout order (judged questions only)."""
    scores = {}
    for (qid, arm), record in verdicts.items():
        scores.setdefault(qid, {})[arm] = record.get("score")
    unknown = sorted(set(scores) - set(order))
    if unknown:
        raise SampleError(f"verdicts for questions outside the holdout: {unknown[:3]}")
    out = {}
    for qid in order:
        if qid in scores:
            stratum = "agree" if len(set(scores[qid].values())) == 1 else "disagree"
            out.setdefault(categories[qid], {s: [] for s in STRATA})[stratum].append(qid)
    return out


def _largest_remainder(total, weights, tie_key):
    """Integer shares of ``total`` proportional to ``weights`` (name -> int); ties by ``tie_key``."""
    size = sum(weights.values())
    quotas = {name: Fraction(total * w, size) for name, w in weights.items()}
    shares = {name: math.floor(q) for name, q in quotas.items()}
    order = sorted(weights, key=lambda name: (-(quotas[name] - shares[name]), tie_key(name)))
    for name in order[:total - sum(shares.values())]:
        shares[name] += 1
    return shares


def allocate(strata, sample_size):
    """{category: {"agree": k, "disagree": k}}: proportional, >= 1 disagree where that stratum exists."""
    sizes = {c: len(s["agree"]) + len(s["disagree"]) for c, s in strata.items()}
    if sample_size <= 0 or sample_size > sum(sizes.values()):
        raise SampleError(f"sample size {sample_size} outside 1..{sum(sizes.values())} judged questions")
    disagree = {c: len(s["disagree"]) for c, s in strata.items()}
    per_category = _largest_remainder(sample_size, sizes, lambda c: (-(disagree[c] > 0), -disagree[c], c))
    out = {}
    for category, k in per_category.items():
        counts = {s: len(strata[category][s]) for s in STRATA}
        split = _largest_remainder(k, counts, lambda s: STRATA.index(s) * -1) if k else {s: 0 for s in STRATA}
        if counts["disagree"] and k and not split["disagree"]:
            split = {"agree": k - 1, "disagree": 1}
        if split["agree"] > counts["agree"]:  # cannot happen proportionally; kept as a hard guard
            split = {"agree": counts["agree"], "disagree": k - counts["agree"]}
        out[category] = split
    return dict(sorted(out.items()))


def draw(strata, allocation, seed, order):
    rng = random.Random(seed)
    chosen = []
    for category in sorted(allocation):
        for stratum in STRATA:
            members = strata[category][stratum]
            chosen.extend(rng.sample(members, allocation[category][stratum]))
    position = {qid: i for i, qid in enumerate(order)}
    return sorted(chosen, key=position.__getitem__)


def mandatory(verdicts, categories):
    """(regressions F/S, regressions F/A, critical non-correct pairs) from the verdict scores."""
    score = {key: record.get("score") for key, record in verdicts.items()}
    qids = sorted({qid for qid, _ in verdicts})
    f_s = [q for q in qids if score.get((q, F)) == "correct" and (q, S) in score and score[(q, S)] != "correct"]
    f_a = [q for q in qids if score.get((q, F)) == "correct" and (q, A) in score and score[(q, A)] != "correct"]
    critical = sorted((q, arm) for (q, arm), s in score.items()
                      if categories[q] in CRITICAL_CATEGORIES and s != "correct")
    return f_s, f_a, critical


def build_list(verdicts, holdout, seed, sample_size, sample_arms):
    order = list(holdout.question_order)
    categories = {q: holdout.questions[q]["category"] for q in order}
    strata = strata_of(verdicts, order, categories)
    allocation = allocate(strata, sample_size)
    sampled = draw(strata, allocation, seed, order)
    f_s, f_a, critical = mandatory(verdicts, categories)
    wanted = {(q, arm) for q in sampled for arm in sample_arms if (q, arm) in verdicts}
    wanted |= {(q, arm) for q in f_s for arm in (F, S)} | {(q, arm) for q in f_a for arm in (F, A)}
    wanted |= set(critical)
    position = {qid: i for i, qid in enumerate(order)}
    arm_order = list(ARMS)
    ordered = sorted(wanted, key=lambda p: (position[p[0]], arm_order.index(p[1])))
    pairs = [{"question_id": q, "arm": arm} for q, arm in ordered if reviewable(verdicts[(q, arm)])]
    excluded = [{"question_id": q, "arm": arm, "score": verdicts[(q, arm)].get("score")}
                for q, arm in ordered if not reviewable(verdicts[(q, arm)])]
    return {"schema": 1, "kind": "plan-8C-review-list", "seed": seed, "sample_size": sample_size,
            "sample_arms": list(sample_arms), "critical_categories": list(CRITICAL_CATEGORIES),
            "holdout_manifest_sha256": holdout.manifest_sha256,
            "strata": {c: {s: len(v[s]) for s in STRATA} for c, v in sorted(strata.items())},
            "allocation": allocation, "sampled_question_ids": sampled,
            "mandatory": {"regressions_f_s": f_s, "regressions_f_a": f_a,
                          "critical_not_correct": [list(p) for p in critical]},
            "excluded_unreviewable": excluded, "pairs": pairs}


def write_list(path, document):
    """Create-only; returns the SHA-256 of the exact bytes the runner will pin."""
    data = (json.dumps(document, indent=1, ensure_ascii=False) + "\n").encode("utf-8")
    descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o644)
    with os.fdopen(descriptor, "wb") as handle:
        handle.write(data)
        handle.flush()
        os.fsync(handle.fileno())
    return hashlib.sha256(data).hexdigest()


def parse_arms(value):
    arms = [a.strip() for a in value.split(",") if a.strip()]
    if not arms or any(a not in ARMS for a in arms) or len(set(arms)) != len(arms):
        raise SampleError(f"--sample-arms must be distinct names from {list(ARMS)}")
    return arms


def main(argv=None, out=None):
    out = out or sys.stdout
    parser = argparse.ArgumentParser(prog="review_sample.py", description=__doc__.splitlines()[0])
    parser.add_argument("--verdicts-dir", required=True, help="a verdicts/ folder (campaign or evidence copy)")
    parser.add_argument("--holdout-dir", required=True)
    parser.add_argument("--holdout-manifest-sha256", required=True)
    parser.add_argument("--always50-sha256", required=True)
    parser.add_argument("--seed", required=True, help="frozen before the draw (e.g. the campaign_id)")
    parser.add_argument("--sample-size", type=int, default=100)
    parser.add_argument("--sample-arms", required=True, help="comma-separated arm names, e.g. facts_retry")
    parser.add_argument("--out", required=True, help="new file (create-only)")
    args = parser.parse_args(argv)
    try:
        holdout = verify_holdout(args.holdout_dir, args.holdout_manifest_sha256, args.always50_sha256)
        document = build_list(load_verdicts(args.verdicts_dir), holdout, args.seed, args.sample_size,
                              parse_arms(args.sample_arms))
        digest = write_list(args.out, document)
    except (SampleError, ValueError, OSError) as exc:
        print(f"refused: {type(exc).__name__}: {exc}", file=sys.stderr)
        return 2
    print(f"{digest}  {args.out}  ({len(document['pairs'])} pair(s), "
          f"{len(document['sampled_question_ids'])} sampled question(s))", file=out)
    return 0


if __name__ == "__main__":
    sys.exit(main())
