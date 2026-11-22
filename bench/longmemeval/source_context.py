"""Experimental gold-blind source lane over sanitized native snapshots.

This is an offline context builder, not a claim of production retrieval parity.
Only original text, query, parent links and original retrieval order are inputs.
"""
import math
import re
from collections import Counter

RELATIVE = re.compile(r'\b(?:(?:\d+|one|two|three|four|five|six|seven|eight|nine|ten|a|an)\s+(?:days?|weeks?|months?|years?|hours?)\s+ago|yesterday|tomorrow|today|last\s+(?:week|month|year)|(?:\d+|jeden|dwa|trzy|cztery|pięć)\s+(?:dni|tygodnie|tygodni|miesiące|miesięcy|lata|lat)\s+temu|wczoraj|dzisiaj|jutro)\b', re.I)
STOP = set('a an the is are was were i my me you your what which how many when did do have has had in on at to of and or for it this that ago'.split())


def normalize(text):
    return ' '.join(text.split()).lower()


def grounded(content, cue, source):
    source = normalize(source)
    return (not cue or normalize(cue) in source) and all(
        normalize(m.group()) in source for m in RELATIVE.finditer(content))


def terms(text):
    return [t for t in re.findall(r'\w+', text.lower()) if t not in STOP]


def windows(source, width=1200):
    """Exact character slices; callers expose UTF-8 byte ranges separately."""
    text = source['content']
    starts = [m.start() for m in re.finditer(r'(?im)^(?:user|assistant|human|ai):', text)]
    starts = sorted(set([0] + starts + [len(text)]))
    for a, b in zip(starts, starts[1:]):
        for offset in range(a, b, width):
            # Preserve the preceding question/answer locally; at most one window.
            left = max(a, offset - 200)
            right = min(len(text), offset + width + 200)
            yield left, right, text[left:right]


def select(query, sources, facts):
    """BM25 source windows + best parent-linked fact reciprocal ranks.

    Equal reciprocal-rank channels, no gold, dates, topic labels or question IDs.
    At most one window per source on the first pass; all remaining windows follow.
    """
    candidates = [(s, a, b, text, Counter(terms(text)))
                  for s in sources for a, b, text in windows(s)]
    if not candidates:
        return []
    qt = set(terms(query))
    n = len(candidates)
    df = Counter(t for *_, counts in candidates for t in qt if counts[t])
    avg = sum(sum(c[-1].values()) for c in candidates) / n or 1
    ranked = []
    for index, (source, a, b, text, counts) in enumerate(candidates):
        length = sum(counts.values())
        score = sum(math.log(1 + (n - df[t] + .5) / (df[t] + .5))
                    * counts[t] * 2.2 / (counts[t] + 1.2 * (.25 + .75 * length / avg))
                    for t in qt if counts[t])
        ranked.append((score, index))
    lexical = {i: rank for rank, (score, i) in enumerate(
        sorted(ranked, key=lambda x: (-x[0], x[1])), 1) if score > 0}
    parents = {}
    for rank, fact in enumerate(facts, 1):
        parents.setdefault(fact.get('source_id'), rank)
    scored = []
    for i, (source, a, b, text, _) in enumerate(candidates):
        # A linked source alone cannot make every generic assistant turn relevant.
        if i not in lexical:
            continue
        score = 1 / (60 + lexical[i])
        if source['id'] in parents:
            score += 1 / (60 + parents[source['id']])
        scored.append((score, source['id'], a, b, text, source))
    scored.sort(key=lambda x: (-x[0], x[1], x[2]))
    seen, first, rest = set(), [], []
    for c in scored:
        (rest if c[1] in seen else first).append(c)
        seen.add(c[1])
    return first + rest
