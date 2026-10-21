"""Prespecified gold-blind full-record selection over one ranked evidence pool.

Only question text, original rank and returned record content are relevance
signals. IDs are opaque; their token cost still belongs to the evidence budget.
Frozen phase4 packer/prompt code is reused unchanged.
"""
import math
import re

from qa_context import count_text, pack_context, render_evidence, serialize_evidence

POLICY = "rank_query_novelty_sqrt_cost_v1"


def _tokens(text):
    return set(re.findall(r"[^\W_]+", text.casefold()))


def _features(question, records):
    tokens = [_tokens(record["content"]) for record in records]
    present = set().union(*tokens)
    terms = sorted(_tokens(question) & present)
    weights = {term: math.log((len(records) + 1) /
                             (1 + sum(term in item for item in tokens)))
               for term in terms}
    return tokens, weights, sum(weights.values())


def _rendered_tokens(records, reader_model):
    return count_text(render_evidence(serialize_evidence(records)), reader_model)


def _pick_next(records, chosen, features, current_tokens, token_budget, reader_model):
    tokens, weights, denominator = features
    selected_df = {term: sum(term in tokens[index] for index in chosen) for term in weights}
    best = None
    for index, _ in enumerate(records):
        if index in chosen:
            continue
        trial = [records[i] for i in sorted(chosen | {index})]
        total = _rendered_tokens(trial, reader_model)
        if total > token_budget:
            continue
        novelty = (sum(weight / math.sqrt(1 + selected_df[term])
                       for term, weight in weights.items() if term in tokens[index])
                   / denominator) if denominator else 0.0
        cost = max(1, total - current_tokens)
        utility = (1 / math.sqrt(index + 1) + novelty) / math.sqrt(cost)
        # Iteration is original rank order: exact utility ties keep the first.
        if best is None or utility > best[1]:
            best = (index, utility, total)
    return best


def _finish(records, chosen, order, anchor, mode, token_budget, reader_model):
    selected = [dict(records[i]) for i in sorted(chosen)]
    packed = pack_context(selected, token_budget, reader_model)
    if packed["dropped_ids"] or packed["selected_ids"] != [r["record_id"] for r in selected]:
        raise ValueError("Canonical packing changed the selected full-record membership")
    dropped = [record for i, record in enumerate(records) if i not in chosen]
    packed["dropped_ids"] = [record["record_id"] for record in dropped]
    packed["dropped_record_tokens"] = {
        record["record_id"]: _rendered_tokens([record], reader_model) for record in dropped}
    packed["selected_records"] = selected
    packed["selection"] = {
        "policy": POLICY, "mode": mode, "candidate_count": len(records),
        "anchor_id": records[anchor]["record_id"] if anchor is not None else None,
        "selected_ranks": [i + 1 for i in sorted(chosen)],
        "greedy_order_ids": [records[i]["record_id"] for i in order],
    }
    return packed


def select_context(question, records, token_budget, reader_model):
    """Return canonical packed context plus selected_records/selection metadata.

    Anchor first full record that fits; repeatedly choose the maximal fixed
    rank-plus-query-novelty utility per square-root incremental wire-token cost.
    Output presentation retains original rank. Records/content are never edited.
    No labels, categories, session annotations or outcome-dependent parameters
    are accepted. The canonical packer performs strict shape/ID/budget checks.
    """
    if not isinstance(question, str):
        raise ValueError("Question must be a string")
    baseline = pack_context(records, token_budget, reader_model)
    # Match the canonical packer's key order before every intermediate count.
    records = [{"record_id": record["record_id"], "content": record["content"]}
               for record in records]
    if not baseline["dropped_ids"]:
        return _finish(records, set(range(len(records))), [], None, "all_fit",
                       token_budget, reader_model)
    if baseline["empty"]:
        return _finish(records, set(), [], None, "none_fit", token_budget, reader_model)
    anchor_id = baseline["selected_ids"][0]
    anchor = next(i for i, record in enumerate(records) if record["record_id"] == anchor_id)
    chosen, order = {anchor}, [anchor]
    features = _features(question, records)
    current_tokens = _rendered_tokens([records[anchor]], reader_model)
    while len(chosen) < len(records):
        best = _pick_next(records, chosen, features, current_tokens, token_budget, reader_model)
        if best is None:
            break
        index, _, current_tokens = best
        chosen.add(index)
        order.append(index)
    return _finish(records, chosen, order, anchor, "greedy", token_budget, reader_model)
