"""Exact-bytes check of the engine's v2 fragment projections against a session.

Verbatim port of ``_marker_role`` / ``_known_turn_ranges`` /
``v2_projection_proven`` from the B1 offline harness
(``full_engine_500_offline_v2.py``): the proof returned by
``POST /v1/evaluation/ingest-readiness-v2`` must slice the exact rendered
transcript bytes, one fragment per Rust-visible turn range, with the receipt's
fragment ids and nothing else. Stdlib only, no I/O.
"""

_ROLES = {'user', 'assistant', 'human', 'ai', 'system'}
# Rust `str::trim` uses Unicode White_Space; Python `str.strip` additionally
# treats U+001C..U+001F as whitespace, which would invent role markers.
_RUST_WHITESPACE = ('\u0009\u000a\u000b\u000c\u000d \u0085  '
                    '           '
                    '    　')
EVIDENCE_V2 = 'fresh_immutable_ingest_projections_v2'


def _marker_role(line):
    """Mirror fragments/turns.rs marker_role for one raw source line."""
    if line.startswith('**'):
        inner = line[2:]
        index = inner.find('**')
        if index < 0:
            return None
        word, rest = inner[:index], inner[index + 2:]
        if word.endswith(':'):
            word = word[:-1]
        elif not rest.startswith(':'):
            return None
    elif line.startswith('['):
        index = line.find(']')
        if index < 0:
            return None
        word = line[1:index]
    else:
        length = 0
        while length < len(line) and line[length].isascii() and line[length].isalpha():
            length += 1
        word, rest = line[:length], line[length:]
        if not (rest.startswith(':') or rest.lstrip(' ').startswith(('—', '–'))):
            return None
    role = word.strip(_RUST_WHITESPACE).translate(str.maketrans(
        'ABCDEFGHIJKLMNOPQRSTUVWXYZ', 'abcdefghijklmnopqrstuvwxyz'))
    if role not in _ROLES:
        return None
    return {'human': 'user', 'ai': 'assistant'}.get(role, role)


def _known_turn_ranges(session, content):
    """Mirror marker-mode Rust turn ranges over the exact rendered session.

    Rendered input begins with `user:` or `assistant:`, so it cannot be JSONL.
    Embedded marker lines remain visible to Rust and are deliberately parsed.
    """
    turns = session.get('turns')
    if not isinstance(turns, list) or not turns:
        return None
    lines = []
    for turn in turns:
        if not isinstance(turn, dict) or turn.get('role') not in ('user', 'assistant') or not isinstance(turn.get('content'), str):
            return None
        lines.append(turn['role'] + ': ' + turn['content'])
    if '\n'.join(lines) != content:
        return None
    total = len(content.encode('utf-8'))
    starts, offset = [], 0
    for raw_line in content.split('\n'):
        role = _marker_role(raw_line.rstrip('\r'))
        if role is not None:
            starts.append((offset, role))
        offset += len(raw_line.encode('utf-8')) + 1
    if len(starts) < 2 or not any(role == 'user' for _, role in starts):
        return [(0, total, None)]
    # The first rendered line always starts with an outer role marker at 0.
    if starts[0][0] != 0:
        return None
    return [(start, starts[index + 1][0] if index + 1 < len(starts) else total, role)
            for index, (start, role) in enumerate(starts)]


def v2_projection_proven(proof, session):
    """Check v2 projections against the exact source bytes and receipt IDs.

    This supplements, but does not replace, the established record verifier or
    prove that a response originates from an authenticated live engine.
    """
    if not isinstance(proof, dict) or not isinstance(session, dict) or not isinstance(session.get('turns'), list):
        return False
    request, response = proof.get('request'), proof.get('response')
    if not isinstance(request, dict) or not isinstance(response, dict):
        return False
    if response.get('schema_version') != 2 or response.get('evidence') != EVIDENCE_V2:
        return False
    source = request.get('source')
    projections = response.get('fragment_projections')
    if not isinstance(source, dict) or not isinstance(projections, list):
        return False
    source_id, content, ids = source.get('source_id'), source.get('content'), source.get('fragment_ids')
    if (not isinstance(source_id, str) or not source_id or not isinstance(content, str)
            or not isinstance(ids, list) or any(not isinstance(i, str) or not i for i in ids)
            or len(ids) != len(set(ids))):
        return False
    encoded = content.encode('utf-8')
    turns = _known_turn_ranges(session, content)
    if turns is None:
        return False
    seen = set()
    ranges = []
    for projection in projections:
        if not isinstance(projection, dict) or set(projection) != {'id', 'content', 'fragment'}:
            return False
        ident, text, meta = projection['id'], projection['content'], projection['fragment']
        if (not isinstance(ident, str) or ident in seen or ident not in ids or not isinstance(text, str)
                or not isinstance(meta, dict) or set(meta) != {
                    'byte_start', 'byte_end', 'conversation_date', 'role', 'source_id', 'turn_index'}):
            return False
        start, end = meta.get('byte_start'), meta.get('byte_end')
        index, role = meta.get('turn_index'), meta.get('role')
        if (type(start) is not int or type(end) is not int or not 0 <= start < end <= len(encoded)
                or type(index) is not int or not 0 <= index < len(turns)
                or role != turns[index][2] or start < turns[index][0] or end > turns[index][1]
                or meta.get('source_id') != source_id or meta.get('conversation_date') != source.get('conversation_date')):
            return False
        try:
            if encoded[start:end].decode('utf-8') != text:
                return False
        except UnicodeDecodeError:
            return False
        seen.add(ident)
        ranges.append((start, end))
    ranges.sort()
    return seen == set(ids) and all(left[1] <= right[0] for left, right in zip(ranges, ranges[1:]))


known_turn_ranges = _known_turn_ranges
