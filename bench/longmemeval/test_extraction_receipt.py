import json
from pathlib import Path
import unittest
from copy import deepcopy

from extraction_receipt import ExtractionCap, confirmed_facts, confirmed_facts_from_mcp_result, confirmed_facts_with_cap
from ingest_receipt import ReceiptError

SOURCE = '00000000-0000-4000-8000-000000000001'
FACT = '00000000-0000-4000-8000-000000000002'


def fixture():
    projection = dict(content='A synthetic event.', level=1, score=1.0, timestamp=100,
                      updated_at=100, consolidated=False, source_ids=[SOURCE],
                      metadata={'_loomem_ingest': dict(version=1, kind='fact', conversation_date=None)},
                      extraction_meta=dict(fact_type='fact', subject=None, event_date=None,
                      event_date_context=None, supersedes=None, superseded_by=None, confidence=.9,
                      extracted_from=SOURCE, extraction_model='synthetic', original_content=None,
                      topic=None, attributed_to=None), source={'agent': 'mcp-ingest'},
                      trust_level='a2', provenance_role='Claim', persistent=True, importance=.9,
                      memory_type='static', valid_from=None, valid_until=None, is_latest=True,
                      superseded_by=None, supersedes_id=None, dormant=False, in_progress=False,
                      deleted_at=None, fragment=None)
    return dict(version=1, scope='generated_facts', evidence='pre_persistence_expected_records_v1',
                source_id=SOURCE, status='complete', error_code=None,
                outcome=dict(input_chunks_total=1, input_chunks_processed=1, calls_attempted=1,
                             calls_completed=1, calls_truncated=0, failed_attempts=0,
                             fact_limit_reached=False, discarded_facts=0, accepted_facts=1),
                persistence=dict(stored=1, failed=0, identity_mismatches=0, invalid_expectations=0),
                facts=[dict(accepted_ordinal=0, id=FACT, allocated_id=FACT, expected=projection)])


class ExtractionReceiptTests(unittest.TestCase):
    def rejects(self, receipt):
        with self.assertRaises(ReceiptError):
            confirmed_facts(receipt, SOURCE)

    def test_shared_fixture_matches_actual_rust_serialization(self):
        root = Path(__file__).resolve().parents[2]
        fixture_path = root / 'loomem-server/src/mcp/dispatcher/ingest/extraction_receipt/fixtures/complete.json'
        structured = json.loads(fixture_path.read_text())
        rows = confirmed_facts(structured['extraction_receipt'], SOURCE)
        self.assertEqual(rows[0]['expected']['importance'], 2.)
        self.assertEqual(rows[0]['expected']['provenance_role'], 'Claim')
        # This wire fixture intentionally has segmentation disabled: E1 must
        # still refuse it even though extraction itself completed correctly.
        with self.assertRaises(ReceiptError):
            confirmed_facts_from_mcp_result({'structuredContent': structured})

    def test_complete_returns_detached_expectations(self):
        receipt = fixture()
        rows = confirmed_facts(receipt, SOURCE)
        self.assertEqual(rows[0]['id'], FACT)
        receipt['facts'][0]['expected']['content'] = 'mutated'
        self.assertEqual(rows[0]['expected']['content'], 'A synthetic event.')

    def test_empty_success_and_exact_final_cap_are_valid(self):
        receipt = fixture()
        receipt['facts'] = []
        receipt['outcome']['accepted_facts'] = receipt['persistence']['stored'] = 0
        self.assertEqual(confirmed_facts(receipt, SOURCE), ())
        receipt = fixture()
        receipt['outcome']['fact_limit_reached'] = True
        self.assertEqual(len(confirmed_facts(receipt, SOURCE)), 1)

    def test_noncomplete_never_becomes_empty_success(self):
        for status in ('partial', 'failed', 'disabled', None):
            with self.subTest(status=status):
                receipt = fixture()
                receipt['status'] = status
                self.rejects(receipt)

    def test_counter_and_coverage_inconsistencies(self):
        for field, value in [('accepted_facts', 2), ('discarded_facts', 1), ('failed_attempts', 1),
                             ('input_chunks_total', 2), ('calls_attempted', 0),
                             ('calls_completed', 2), ('calls_truncated', 2),
                             ('fact_limit_reached', 1), ('accepted_facts', True)]:
            with self.subTest(field=field, value=value):
                receipt = fixture()
                receipt['outcome'][field] = value
                self.rejects(receipt)
        for field in ('failed', 'identity_mismatches', 'invalid_expectations', 'stored'):
            receipt = fixture()
            receipt['persistence'][field] = 2
            self.rejects(receipt)

    def test_identity_ordinal_missing_or_duplicate(self):
        for field, value in [('id', SOURCE), ('allocated_id', SOURCE), ('id', 'invalid'),
                             ('accepted_ordinal', True), ('accepted_ordinal', 1), ('expected', None)]:
            receipt = fixture()
            receipt['facts'][0][field] = value
            self.rejects(receipt)
        receipt = fixture()
        receipt['facts'].append(deepcopy(receipt['facts'][0]))
        receipt['facts'][1]['accepted_ordinal'] = 1
        receipt['outcome']['accepted_facts'] = receipt['persistence']['stored'] = 2
        self.rejects(receipt)

    def test_projection_drift_and_private_extra_rejected(self):
        for field, value in [('source_ids', [FACT]), ('level', True), ('score', float('nan')),
                             ('importance', float('inf')), ('persistent', False),
                             ('provenance_role', 'evidence'), ('source', {'agent': 'foreign'}),
                             ('fragment', {}), ('deleted_at', 100), ('ingester_user_id', 'private')]:
            receipt = fixture()
            receipt['facts'][0]['expected'][field] = value
            self.rejects(receipt)
        receipt = fixture()
        receipt['facts'][0]['expected']['extraction_meta']['extracted_from'] = FACT
        self.rejects(receipt)

    def test_unrecovered_and_recovered_truncation(self):
        receipt = fixture()
        receipt['outcome'].update(calls_completed=0, calls_truncated=1)
        self.rejects(receipt)
        receipt['outcome'].update(calls_attempted=3, calls_completed=2)
        self.assertEqual(len(confirmed_facts(receipt, SOURCE)), 1)
        receipt['outcome'].update(calls_attempted=2, calls_completed=2, calls_truncated=0)
        self.rejects(receipt)
        receipt['outcome'].update(input_chunks_total=0, input_chunks_processed=0)
        self.rejects(receipt)

    def test_extreme_numbers_and_initial_temporal_consistency(self):
        for field, value in [('score', 10**1000), ('importance', 10**1000),
                             ('valid_until', 100), ('updated_at', 101), ('score', 2.),
                             ('importance', .8), ('valid_from', 100)]:
            receipt = fixture()
            receipt['facts'][0]['expected'][field] = value
            self.rejects(receipt)
        receipt = fixture()
        receipt['facts'][0]['expected']['extraction_meta']['confidence'] = 10**1000
        self.rejects(receipt)

    def test_combined_receipts_require_both_complete_same_source(self):
        result = {'structuredContent': {
            'ingest_receipt': dict(version=1, scope='source_and_fragments', status='complete',
                source_id=SOURCE, fragment_ids=[SOURCE + ':f0'],
                fragments=dict(stored=1, failed=0, skipped=0, segmentation_status='complete')),
            'extraction_receipt': fixture()}}
        self.assertEqual(len(confirmed_facts_from_mcp_result(result)), 1)
        for key, value in [('status', 'partial'), ('source_id', FACT)]:
            broken = deepcopy(result)
            broken['structuredContent']['ingest_receipt'][key] = value
            with self.assertRaises(ReceiptError):
                confirmed_facts_from_mcp_result(broken)
        result['isError'] = True
        with self.assertRaises(ReceiptError):
            confirmed_facts_from_mcp_result(result)

    def capped(self, **outcome):
        """The engine's fact-cap receipt (status partial, no error code), with outcome overrides."""
        receipt = fixture()
        receipt['status'] = 'partial'
        receipt['outcome'].update({'fact_limit_reached': True, 'discarded_facts': 2, **outcome})
        return receipt

    def test_fact_limit_partial_is_accepted_only_with_cap(self):
        rows, cap = confirmed_facts_with_cap(self.capped(), SOURCE)
        self.assertEqual((len(rows), cap), (1, ExtractionCap(True, 2)))
        self.assertEqual(rows[0]['id'], FACT)
        self.rejects(self.capped())  # confirmed_facts itself still refuses every partial
        rows, cap = confirmed_facts_with_cap(fixture(), SOURCE)
        self.assertEqual((len(rows), cap), (1, ExtractionCap(False, 0)))
        exact = fixture()
        exact['outcome']['fact_limit_reached'] = True  # complete, limit reached, nothing discarded
        self.assertEqual(confirmed_facts_with_cap(exact, SOURCE)[1], ExtractionCap(False, 0))

    def test_fact_limit_partial_refusals(self):
        unflagged = self.capped(fact_limit_reached=False)
        nothing_discarded = self.capped(discarded_facts=0)
        complete_discarded = fixture()
        complete_discarded['outcome'].update(fact_limit_reached=True, discarded_facts=2)
        with_error = self.capped()
        with_error['error_code'] = 'all_chunks_failed'
        failed_status = self.capped()
        failed_status['status'] = 'failed'
        unwritten = self.capped()
        unwritten['persistence']['failed'] = 1
        cases = {'partial without limit flag': unflagged, 'partial without discards': nothing_discarded,
                 'failed attempt': self.capped(failed_attempts=1), 'error code': with_error,
                 'unprocessed chunk': self.capped(input_chunks_total=2), 'complete with discards': complete_discarded,
                 'failed status': failed_status, 'unconfirmed write': unwritten}
        for label, receipt in cases.items():
            with self.subTest(case=label):
                with self.assertRaises(ReceiptError):
                    confirmed_facts_with_cap(receipt, SOURCE)

    def test_source_scope_schema_and_error_bindings(self):
        for field, value in [('version', True), ('source_id', FACT), ('scope', 'source_and_fragments'),
                             ('error_code', 'all_chunks_failed'), ('extra', 1)]:
            receipt = fixture()
            receipt[field] = value
            self.rejects(receipt)


if __name__ == '__main__':
    unittest.main()
