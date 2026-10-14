"""Bounded, gold-blind promotions over canonical ranked first-fit evidence."""
from dataclasses import dataclass
import math
import re

from qa_context import count_text, pack_context, render_evidence, serialize_evidence

POLICY = "bounded_rank_query_novelty_v2"
PROTECTED_RANK = 20
MAX_PROMOTIONS = 2
MAX_PROMOTED_TOKENS = 800


@dataclass
class _Scoring:
    record_tokens: list
    weights: dict
    denominator: float
    standalone: list


@dataclass
class _Selection:
    records: list
    baseline: dict
    baseline_chosen: set
    chosen: set
    promotions: list
    evicted: list


@dataclass
class _Policy:
    scoring: _Scoring
    baseline_utility: dict
    token_budget: int
    reader_model: str


def _tokens(text):
    return set(re.findall(r"[^\W_]+", text.casefold()))


def _rendered_tokens(records, reader_model):
    return count_text(render_evidence(serialize_evidence(records)), reader_model)


def _features(question, records, reader_model):
    record_tokens = [_tokens(record["content"]) for record in records]
    present = set().union(*record_tokens)
    terms = sorted(_tokens(question) & present)
    weights = {
        term: math.log((len(records) + 1) /
                       (1 + sum(term in tokens for tokens in record_tokens)))
        for term in terms
    }
    standalone = [_rendered_tokens([record], reader_model) for record in records]
    return _Scoring(record_tokens, weights, sum(weights.values()), standalone)


def _term_counts(chosen, scoring, excluded=None):
    return {
        term: sum(term in scoring.record_tokens[index]
                  for index in chosen if index != excluded)
        for term in scoring.weights
    }


def _utility(index, scoring, selected_counts):
    rank_prior = 1 / math.sqrt(index + 1)
    novelty = (sum(weight for term, weight in scoring.weights.items()
                   if term in scoring.record_tokens[index] and selected_counts[term] < 2)
               / scoring.denominator) if scoring.denominator else 0.0
    effective_novelty = min(novelty, rank_prior)
    return ((rank_prior + effective_novelty) /
            math.sqrt(max(1, scoring.standalone[index])))


def _record_ids(records, indices):
    return [records[index]["record_id"] for index in indices]


def _promotion_exposure(records, chosen, promotions, reader_model):
    with_promotions = [records[index] for index in sorted(chosen)]
    counterfactual = [records[index] for index in sorted(chosen - set(promotions))]
    return max(0, (_rendered_tokens(with_promotions, reader_model) -
                   _rendered_tokens(counterfactual, reader_model)))


def _selection_metadata(state, exposure):
    selected_indices = sorted(state.chosen)
    return {
        "policy": POLICY,
        "mode": "all_fit" if not state.baseline["dropped_ids"] else "bounded_promotions",
        "candidate_count": len(state.records),
        "protected_rank_max": PROTECTED_RANK,
        "selected_ranks": [index + 1 for index in selected_indices],
        "baseline_selected_ids": list(state.baseline["selected_ids"]),
        "promoted_ids": _record_ids(state.records, state.promotions),
        "evicted_baseline_ids": _record_ids(state.records, state.evicted),
        "promoted_exposure_tokens": exposure,
        "promotion_limit": MAX_PROMOTIONS,
        "promotion_token_limit": MAX_PROMOTED_TOKENS,
    }


def _finish(state, token_budget, reader_model):
    selected = [dict(state.records[index]) for index in sorted(state.chosen)]
    packed = pack_context(selected, token_budget, reader_model)
    expected_ids = [record["record_id"] for record in selected]
    if packed["dropped_ids"] or packed["selected_ids"] != expected_ids:
        raise ValueError("Canonical packing changed the selected full-record membership")
    dropped = [record for index, record in enumerate(state.records)
               if index not in state.chosen]
    packed["dropped_ids"] = [record["record_id"] for record in dropped]
    packed["dropped_record_tokens"] = {
        record["record_id"]: _rendered_tokens([record], reader_model)
        for record in dropped
    }
    exposure = _promotion_exposure(
        state.records, state.chosen, state.promotions, reader_model
    )
    packed["selected_records"] = selected
    packed["selection"] = _selection_metadata(state, exposure)
    return packed


def _initial_state(records, baseline):
    records = [{"record_id": record["record_id"], "content": record["content"]}
               for record in records]
    baseline_ids = set(baseline["selected_ids"])
    chosen = {index for index, record in enumerate(records)
              if record["record_id"] in baseline_ids}
    return _Selection(records, baseline, set(chosen), chosen, [], [])


def _baseline_utilities(state, scoring):
    utilities = {}
    for index in state.baseline_chosen:
        if index + 1 <= PROTECTED_RANK:
            continue
        counts = _term_counts(state.baseline_chosen, scoring, excluded=index)
        utilities[index] = _utility(index, scoring, counts)
    return utilities


def _smallest_fitting_eviction(state, candidate, removable, token_budget, reader_model):
    """Find the shortest lowest-utility eviction prefix with bounded repacks."""
    cache = {}

    def fits(count):
        if count not in cache:
            trial = (state.chosen - set(removable[:count])) | {candidate}
            packed = [state.records[index] for index in sorted(trial)]
            cache[count] = _rendered_tokens(packed, reader_model) <= token_budget
        return cache[count]

    if fits(0):
        return []
    if not removable or not fits(len(removable)):
        return None
    low, high = 1, len(removable)
    while low < high:
        middle = (low + high) // 2
        if fits(middle):
            high = middle
        else:
            low = middle + 1
    return removable[:low]


def _removable(state, policy, candidate_utility):
    eligible = (index for index in state.baseline_chosen & state.chosen
                if index + 1 > PROTECTED_RANK
                and policy.baseline_utility[index] < candidate_utility)
    return sorted(eligible,
                  key=lambda index: (policy.baseline_utility[index], -index))


def _try_candidate(state, candidate, candidate_utility, policy):
    removable = _removable(state, policy, candidate_utility)
    removed = _smallest_fitting_eviction(
        state, candidate, removable, policy.token_budget, policy.reader_model
    )
    if removed is None:
        return False
    tentative = (state.chosen - set(removed)) | {candidate}
    promotions = state.promotions + [candidate]
    exposure = _promotion_exposure(
        state.records, tentative, promotions, policy.reader_model
    )
    if exposure > MAX_PROMOTED_TOKENS:
        return False
    state.chosen = tentative
    state.promotions = promotions
    state.evicted.extend(removed)
    return True


def _ranked_outsiders(state, outsiders, scoring):
    counts = _term_counts(state.chosen, scoring)
    return sorted((-_utility(index, scoring, counts), index) for index in outsiders)


def _run_promotions(state, policy):
    outsiders = [index for index in range(len(state.records))
                 if index not in state.baseline_chosen]
    while outsiders and len(state.promotions) < MAX_PROMOTIONS:
        accepted = None
        for negative_utility, candidate in _ranked_outsiders(
                state, outsiders, policy.scoring):
            if _try_candidate(state, candidate, -negative_utility, policy):
                accepted = candidate
                break
        if accepted is None:
            return
        outsiders.remove(accepted)


def select_context(question, records, token_budget, reader_model):
    """Pack baseline evidence and apply at most two bounded lexical promotions."""
    if not isinstance(question, str):
        raise ValueError("Question must be a string")
    baseline = pack_context(records, token_budget, reader_model)
    state = _initial_state(records, baseline)
    if not baseline["dropped_ids"] or baseline["empty"]:
        return _finish(state, token_budget, reader_model)
    scoring = _features(question, state.records, reader_model)
    policy = _Policy(scoring, _baseline_utilities(state, scoring),
                     token_budget, reader_model)
    _run_promotions(state, policy)
    return _finish(state, token_budget, reader_model)
