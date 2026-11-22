"""Validate the ingest expected-set manifest; this is NOT index readiness proof.

Only source and successfully persisted fragment IDs are covered. Extraction facts,
record generations and index publication still require their own evidence.
"""
import re
from uuid import UUID


class ReceiptError(ValueError):
    """Receipt cannot establish the benchmark's exact expected record set."""


def _object(value, fields, label):
    if not isinstance(value, dict) or set(value) != set(fields):
        raise ReceiptError(f'{label}: unexpected or missing fields')
    return value


def _counter(value, label):
    if type(value) is not int or value < 0:
        raise ReceiptError(f'{label}: expected nonnegative integer')
    return value


def expected_ids_from_receipt(receipt):
    """Return immutable exact IDs from a complete, segmented v1 receipt.

    This benchmark deliberately rejects disabled segmentation even though the
    server may legitimately report such an ingest as complete.
    """
    receipt = _object(receipt, ('version', 'scope', 'status', 'source_id',
                                'fragment_ids', 'fragments'), 'ingest_receipt')
    if type(receipt['version']) is not int or receipt['version'] != 1:
        raise ReceiptError('unsupported receipt version')
    if receipt['scope'] != 'source_and_fragments':
        raise ReceiptError('unsupported receipt scope')
    if receipt['status'] != 'complete':
        raise ReceiptError('ingest receipt is not complete')
    source = receipt['source_id']
    try:
        canonical = str(UUID(source)) if isinstance(source, str) else None
    except (ValueError, AttributeError):
        canonical = None
    if canonical is None or source != canonical:
        raise ReceiptError('source_id must be a canonical UUID')
    counts = _object(receipt['fragments'], ('stored', 'failed', 'skipped',
                     'segmentation_status'), 'fragments')
    for key in ('stored', 'failed', 'skipped'):
        _counter(counts[key], key)
    if counts['segmentation_status'] != 'complete':
        raise ReceiptError('fragment segmentation is not complete')
    if counts['failed'] or counts['skipped']:
        raise ReceiptError('fragment failures or skips make ingest incomplete')
    fragments = receipt['fragment_ids']
    if not isinstance(fragments, list):
        raise ReceiptError('fragment_ids must be an array')
    pattern = re.compile(re.escape(source) + r':f(?:0|[1-9][0-9]*)\Z')
    if any(not isinstance(item, str) or not pattern.fullmatch(item)
           for item in fragments):
        raise ReceiptError('invalid fragment ID or foreign source prefix')
    if len(set(fragments)) != len(fragments):
        raise ReceiptError('duplicate fragment IDs')
    if counts['stored'] != len(fragments):
        raise ReceiptError('stored count does not match exact fragment IDs')
    return frozenset([source, *fragments])


def expected_ids_from_mcp_result(result):
    """Read structuredContent only; never infer IDs from legacy human text."""
    if not isinstance(result, dict) or result.get('isError', False) is not False:
        raise ReceiptError('MCP result is an error or malformed')
    structured = result.get('structuredContent')
    if not isinstance(structured, dict) or 'ingest_receipt' not in structured:
        raise ReceiptError('missing structured ingest receipt')
    return expected_ids_from_receipt(structured['ingest_receipt'])
