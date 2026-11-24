"""Fail-closed generated-fact write receipts for the private evaluation build.

This validates the expected set, not index/vector readiness or semantic recall.
It is intentionally separate from the extraction-disabled E1 adapter.
"""
import math
from copy import deepcopy
from dataclasses import dataclass
from uuid import UUID

from ingest_receipt import ReceiptError, _counter, _object


OUTCOME_FIELDS = ('input_chunks_total', 'input_chunks_processed', 'calls_attempted',
                  'calls_completed', 'calls_truncated', 'failed_attempts',
                  'fact_limit_reached', 'discarded_facts', 'accepted_facts')
PERSISTENCE_FIELDS = ('stored', 'failed', 'identity_mismatches', 'invalid_expectations')


def _uuid(value):
    try:
        return isinstance(value, str) and str(UUID(value)) == value
    except (ValueError, AttributeError):
        return False


@dataclass(frozen=True)
class ExtractionCap:
    """``capped``: the per-chunk fact cap discarded ``discarded_facts`` otherwise eligible facts."""
    capped: bool
    discarded_facts: int


def confirmed_facts(receipt, source_id):
    """Return detached fact rows from a complete process/write receipt.

    Caller must still prove storage/index/vector readiness before using these rows as benchmark evidence.
    """
    return _confirmed(receipt, source_id, False)[0]


def confirmed_facts_with_cap(receipt, source_id):
    """``confirmed_facts`` that also accepts the engine's fact-cap ``partial``: no error code, every input
    chunk processed, no failed attempt, ``fact_limit_reached`` with ``discarded_facts > 0`` and every other
    check as for ``complete``. Returns ``(rows, ExtractionCap)``; any other ``partial`` still raises."""
    return _confirmed(receipt, source_id, True)


def _confirmed(receipt, source_id, allow_fact_limit):
    _object(receipt, ('version', 'scope', 'evidence', 'source_id', 'status',
                     'outcome', 'persistence', 'facts', 'error_code'), 'extraction_receipt')
    if type(receipt['version']) is not int or receipt['version'] != 1:
        raise ReceiptError('unsupported extraction version')
    if (receipt['scope'] != 'generated_facts' or
            receipt['evidence'] != 'pre_persistence_expected_records_v1'):
        raise ReceiptError('unsupported extraction evidence')
    if not _uuid(source_id) or receipt['source_id'] != source_id:
        raise ReceiptError('extraction source binding mismatch')
    capped = allow_fact_limit and receipt['status'] == 'partial' and receipt['error_code'] is None
    if not capped and (receipt['status'] != 'complete' or receipt['error_code'] is not None):
        raise ReceiptError('extraction process is not complete')
    outcome = _object(receipt['outcome'], OUTCOME_FIELDS, 'outcome')
    for key in OUTCOME_FIELDS:
        if key != 'fact_limit_reached':
            _counter(outcome[key], key)
    if type(outcome['fact_limit_reached']) is not bool:
        raise ReceiptError('invalid cap flag')
    if (outcome['input_chunks_processed'] != outcome['input_chunks_total'] or
            outcome['failed_attempts'] or (outcome['discarded_facts'] and not capped)):
        raise ReceiptError('incomplete input or discarded facts')
    if capped and not (outcome['fact_limit_reached'] and outcome['discarded_facts']):
        raise ReceiptError('partial extraction is not a fact-limit cap')
    if (outcome['input_chunks_total'] < 1 or
            outcome['calls_completed'] != outcome['input_chunks_processed'] + outcome['calls_truncated'] or
            outcome['calls_truncated'] > 2 * outcome['input_chunks_processed'] or
            outcome['calls_attempted'] != outcome['calls_completed'] + outcome['calls_truncated'] or
            outcome['calls_completed'] > outcome['calls_attempted'] or
            outcome['calls_truncated'] > outcome['calls_attempted'] or
            outcome['calls_attempted'] < outcome['input_chunks_processed']):
        raise ReceiptError('inconsistent call counts')
    persistence = _object(receipt['persistence'], PERSISTENCE_FIELDS, 'persistence')
    for key in PERSISTENCE_FIELDS:
        _counter(persistence[key], key)
    if any(persistence[key] for key in PERSISTENCE_FIELDS if key != 'stored'):
        raise ReceiptError('unconfirmed fact writes')
    facts = receipt['facts']
    if not isinstance(facts, list) or len(facts) != persistence['stored']:
        raise ReceiptError('stored count does not match fact rows')
    if len(facts) != outcome['accepted_facts']:
        raise ReceiptError('accepted facts are missing from receipt')
    seen = {source_id}
    for ordinal, fact in enumerate(facts):
        _object(fact, ('accepted_ordinal', 'id', 'allocated_id', 'expected'), 'fact')
        if type(fact['accepted_ordinal']) is not int or fact['accepted_ordinal'] != ordinal:
            raise ReceiptError('fact ordinal missing, duplicated or out of order')
        if (not _uuid(fact['id']) or fact['id'] != fact['allocated_id'] or
                fact['id'] in seen):
            raise ReceiptError('fact identity mismatch or duplicate')
        seen.add(fact['id'])
        _projection(fact['expected'], source_id)
    return tuple(deepcopy(facts)), ExtractionCap(capped, outcome['discarded_facts'])


PROJECTION_FIELDS = (
    'content', 'level', 'score', 'timestamp', 'updated_at', 'consolidated',
    'source_ids', 'metadata', 'extraction_meta', 'source', 'trust_level',
    'provenance_role', 'persistent', 'importance', 'memory_type', 'valid_from',
    'valid_until', 'is_latest', 'superseded_by', 'supersedes_id', 'dormant',
    'in_progress', 'deleted_at', 'fragment',
)


def _finite(number):
    if type(number) not in (int, float):
        return False
    try:
        return math.isfinite(number)
    except OverflowError:
        return False


def _projection(value, source_id):
    _object(value, PROJECTION_FIELDS, 'expected')
    if not isinstance(value['content'], str):
        raise ReceiptError('expected content must be text')
    for name in ('timestamp', 'updated_at'):
        _counter(value[name], name)
        if value[name] > 2**64 - 1:
            raise ReceiptError('timestamp outside Rust u64 range')
    for name in ('valid_from', 'valid_until', 'deleted_at'):
        if value[name] is not None:
            _counter(value[name], name)
    for name in ('score', 'importance'):
        number = value[name]
        if not _finite(number):
            raise ReceiptError('nonfinite expected number')
    if type(value['level']) is not int or value['level'] != 1:
        raise ReceiptError('expected record is not a generated fact')
    for name, expected in (('consolidated', False), ('persistent', True),
                           ('is_latest', True), ('dormant', False), ('in_progress', False)):
        if value[name] is not expected:
            raise ReceiptError('invalid initial fact lifecycle')
    if (value['source_ids'] != [source_id] or value['provenance_role'] != 'Claim' or
            value['trust_level'] != 'a2' or value['memory_type'] not in ('static', 'dynamic')):
        raise ReceiptError('invalid fact provenance')
    if value['score'] != 1 or value['updated_at'] != value['timestamp']:
        raise ReceiptError('unexpected initial fact score or update time')
    for name in ('valid_until', 'superseded_by', 'supersedes_id', 'deleted_at', 'fragment'):
        if value[name] is not None:
            raise ReceiptError('unexpected initial fact state')
    marker = _object(value['metadata'], ('_loomem_ingest',), 'metadata')['_loomem_ingest']
    _object(marker, ('version', 'kind', 'conversation_date'), 'marker')
    if type(marker['version']) is not int or marker['version'] != 1 or marker['kind'] != 'fact':
        raise ReceiptError('invalid fact marker')
    meta = value['extraction_meta']
    if not isinstance(meta, dict) or meta.get('extracted_from') != source_id:
        raise ReceiptError('invalid extraction source binding')
    confidence = meta.get('confidence')
    if not _finite(confidence) or confidence < .5:
        raise ReceiptError('invalid extraction confidence')
    if value['source'] != {'agent': 'mcp-ingest'}:
        raise ReceiptError('invalid fact source tag')
    _object(meta, ('fact_type', 'subject', 'event_date', 'event_date_context',
                  'supersedes', 'superseded_by', 'confidence', 'extracted_from',
                  'extraction_model', 'original_content', 'topic', 'attributed_to'), 'extraction_meta')
    if meta['fact_type'] not in ('preference_or_decision', 'project_state', 'fact', 'event', 'experience'):
        raise ReceiptError('unknown fact type')
    for name in ('subject', 'event_date', 'event_date_context', 'extraction_model', 'topic', 'attributed_to'):
        if meta[name] is not None and not isinstance(meta[name], str):
            raise ReceiptError('invalid extraction text field')
    for name in ('supersedes', 'superseded_by', 'original_content'):
        if meta[name] is not None:
            raise ReceiptError('unexpected rewritten fact')
    date = marker['conversation_date']
    if date is not None:
        from datetime import date as Date
        try:
            if not isinstance(date, str) or Date.fromisoformat(date).isoformat() != date:
                raise ValueError
        except ValueError as error:
            raise ReceiptError('invalid conversation date') from error

    importance = max(2., confidence) if meta['fact_type'] == 'preference_or_decision' else confidence
    if value['importance'] != importance:
        raise ReceiptError('importance differs from generated fact')
    from datetime import date as Date
    event_date = meta['event_date']
    try:
        parsed = Date.fromisoformat(event_date) if event_date is not None else None
        if parsed is not None and parsed.isoformat() != event_date:
            raise ValueError
    except (TypeError, ValueError) as error:
        raise ReceiptError('invalid event date') from error
    epoch = Date(1970, 1, 1)
    valid_from = (parsed - epoch).days * 86400 if parsed is not None and parsed >= epoch else None
    if value['valid_from'] != valid_from:
        raise ReceiptError('event date and validity differ')


def confirmed_facts_from_mcp_result(result):
    """Require complete source/fragment and extraction receipts for the same source."""
    from ingest_receipt import expected_ids_from_mcp_result
    expected_ids_from_mcp_result(result)
    structured = result['structuredContent']
    if 'extraction_receipt' not in structured:
        raise ReceiptError('missing generated-fact receipt')
    return confirmed_facts(structured['extraction_receipt'],
                           structured['ingest_receipt']['source_id'])
