import unittest
from evidence_gate import source_need, validate_diagnostic, normalize_diagnostic


def fact(identity='f1', content='The silver lantern is in the attic.', source_id='s1'):
    """Minimal v2 fact record (qa_context_v2.validate_records_v2 shape)."""
    return {'record_id': identity, 'content': content, 'author': 'user',
            'conversation_date': '2024-05-01', 'event_date': None,
            'event_date_provenance': None, 'source_id': source_id,
            'byte_start': None, 'byte_end': None}


def diagnostic(**extra):
    return {'answer': 'In the attic.', 'coverage': 'complete', 'missing_parts': [],
            'conflict_fact_ids': [], 'supporting_fact_ids': ['f1'], **extra}


class EvidenceGateTests(unittest.TestCase):
    def test_supported_complete_does_not_expand(self):
        self.assertFalse(source_need(diagnostic(), [fact()])['required'])

    def test_missing_part_and_uncited_claim_expand(self):
        for value in [diagnostic(coverage='missing', missing_parts=['room']),
                      diagnostic(supporting_fact_ids=[])]:
            self.assertTrue(source_need(value, [fact()])['required'])

    def test_refusal_paraphrases_override_complete_status(self):
        for text in ['There is no evidence of the location.', "I can't tell the breed.",
                     'The room is not stated.', 'Brak danych o pokoju.', 'Nie podano daty.']:
            with self.subTest(text=text):
                self.assertIn('answer_reports_gap', source_need(diagnostic(answer=text), [fact()])['reasons'])

    def test_conflict_preserves_both_target_ids(self):
        fs = [fact(), fact('f2')]
        value = diagnostic(coverage='conflict', conflict_fact_ids=['f1', 'f2'])
        self.assertEqual(source_need(value, fs)['target_fact_ids'], ['f1', 'f2'])

    def test_explicit_missing_diagnostic_can_have_empty_prose_without_guessing(self):
        value = diagnostic(answer='', coverage='missing', missing_parts=['tree variety'], supporting_fact_ids=[])
        self.assertTrue(source_need(value, [fact()])['required'])
        with self.assertRaises(ValueError):
            source_need(diagnostic(answer=''), [fact()])

    def test_schema_and_unknown_citations_fail_closed(self):
        for value in [diagnostic(supporting_fact_ids=['invented']), diagnostic(coverage='missing'),
                      diagnostic(coverage='conflict', conflict_fact_ids=['f1', 'f1']),
                      diagnostic(missing_parts=['room']), diagnostic(extra='gold')]:
            with self.subTest(value=value), self.assertRaises(ValueError):
                validate_diagnostic(value, [fact()])

    def test_unknown_citations_route_to_sources_without_guessing_a_handle(self):
        raw = diagnostic(supporting_fact_ids=['f1', 'invented'])
        checked, audit = normalize_diagnostic(raw, [fact()])
        self.assertEqual(checked['supporting_fact_ids'], ['f1'])
        self.assertEqual(raw['supporting_fact_ids'], ['f1', 'invented'])
        self.assertTrue(source_need(checked, [fact()])['required'])
        self.assertEqual(audit['invalid_fact_ids_discarded'], ['invented'])

    def test_inconsistent_empty_complete_diagnostic_routes_to_sources(self):
        checked, audit = normalize_diagnostic(diagnostic(answer=''), [fact()])
        self.assertEqual(checked['coverage'], 'missing')
        self.assertTrue(source_need(checked, [fact()])['required'])

    def test_valid_diagnostics_remain_identical_for_replay(self):
        raw = diagnostic()
        checked, audit = normalize_diagnostic(raw, [fact()])
        self.assertEqual(checked, raw)
        self.assertFalse(audit['diagnostic_routed_to_missing'])
