import copy
import unittest

from evidence_gate import normalize_diagnostic
from gate_verifier import CHECKS, messages, verified_need
from test_evidence_gate import diagnostic, fact


class GateVerifierTests(unittest.TestCase):
    def setUp(self):
        self.facts = [fact()]
        self.diagnostic = diagnostic()
        self.audit = {**dict.fromkeys(CHECKS, True), 'issues': []}

    def need(self, question='Where is my teapot?', audit=None):
        return verified_need(question, self.diagnostic, self.facts, audit or self.audit)

    def test_supported_simple_answer_keeps_sources_closed(self):
        self.assertFalse(self.need()['required'])

    def test_each_failed_check_independently_opens_sources(self):
        for key in CHECKS:
            with self.subTest(key=key):
                self.assertTrue(self.need(audit={**self.audit, key: False})['required'])

    def test_counts_totals_chronology_and_updates_require_originals(self):
        for question in ['How many siblings do I have?', 'Which event happened first?',
                         'How long did I work before joining?', 'What is my current balance?',
                         'What percentage discount did I get?', 'Did I attend all meetings?']:
            with self.subTest(question=question):
                self.assertTrue(self.need(question)['required'])

    def test_reported_issue_overrides_all_true_checks(self):
        self.assertTrue(self.need(audit={**self.audit, 'issues': ['Only sisters counted.']})['required'])

    def test_no_mutation_or_gold_fields(self):
        before = copy.deepcopy([self.facts, self.diagnostic, self.audit])
        sent = messages('Where?', self.diagnostic, self.facts)
        self.need()
        self.assertEqual([self.facts, self.diagnostic, self.audit], before)
        self.assertNotIn('reference_answer', sent[1]['content'])
        with self.assertRaises(ValueError):
            self.need(audit={**self.audit, 'gold': 'x'})

    def test_untyped_or_malformed_audit_is_rejected(self):
        for bad in [{**self.audit, CHECKS[0]: 1}, {**self.audit, 'issues': ['']}, {}]:
            with self.assertRaises(ValueError):
                verified_need('Where?', self.diagnostic, self.facts, bad)

    def test_existing_gap_cannot_be_overruled(self):
        d, _ = normalize_diagnostic(diagnostic(supporting_fact_ids=[]), self.facts)
        self.assertTrue(verified_need('Where?', d, self.facts, self.audit)['required'])

    def test_final_decisions_require_original_sequence_even_with_full_audit_pass(self):
        for question in ['Which cafe did I ultimately settle on?',
                         'Which plan did we finally agree on?',
                         'What color did I eventually choose?',
                         'Which camera did I end up buying?']:
            with self.subTest(question=question):
                self.assertIn('final_choice_requires_original_sequence', self.need(question)['reasons'])
