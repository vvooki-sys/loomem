import contextlib
import io
import hashlib
import json
from pathlib import Path
import tempfile
import unittest

import validate as target


class ValidatorTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.dataset = self.write('dataset.json', [
            {'question_id': 'synthetic_a', 'question_type': 'single-session-user'},
            {'question_id': 'synthetic_b_abs', 'question_type': 'knowledge-update'},
            {'question_id': 'synthetic_c', 'question_type': 'knowledge-update'}])
        # Hashes are derived only from explicit synthetic fixture strings.
        self.settings = {
            'engine_sha': hashlib.sha1(b'synthetic engine').hexdigest(),
            'harness_sha': hashlib.sha1(b'synthetic harness').hexdigest(),
            'reader_model': 'synthetic-reader', 'judge_model': 'synthetic-judge',
            'embedding_model': 'synthetic-embedding', 'context_budget_tokens': 2048,
            'seed': 17,
            **{field: hashlib.sha256(field.encode()).hexdigest() for field in
               ('reader_prompt_sha256', 'judge_prompt_sha256', 'execution_config_sha256')}}
        self.config = self.write('config.json', self.settings)
        manifest = target.build_manifest(self.dataset, self.config)
        self.manifest = self.write('manifest.json', manifest)
        self.binding = target.canonical_hash(manifest)
        self.results = self.root / 'results.jsonl'

    def write(self, name, value):
        path = self.root / name
        path.write_text(json.dumps(value))
        return path

    def row(self, qid='synthetic_a', **extra):
        return {'question_id': qid, 'manifest_sha256': self.binding,
                'correct': True, **extra}

    def run_rows(self, rows, partial=True):
        self.results.write_text(''.join(json.dumps(row) + '\n' for row in rows))
        return target.validate(self.dataset, self.config, self.manifest,
                               self.results, partial)

    def test_complete_denominator_and_slices(self):
        failed = self.row('synthetic_c', status='failed', failure_stage='ingest')
        del failed['correct']
        report = self.run_rows([self.row(), self.row('synthetic_b_abs', correct=False), failed], False)
        self.assertTrue(report['accepted_complete_run'])
        self.assertEqual(report['summary']['accuracy'], 1 / 3)
        self.assertEqual(report['summary']['completion_rate'], 2 / 3)
        self.assertEqual(report['summary']['failed'], 1)
        self.assertEqual(report['abstentions']['correct'], 0)
        self.assertEqual(report['categories']['knowledge-update']['expected'], 2)
        self.assertEqual(report['retry_failed_ids'], ['synthetic_c'])
        self.assertNotIn('synthetic_c', report['resume_skip_ids'])
        self.assertEqual(report['results_sha256'], target.sha256(self.results))

    def test_missing_rejected_default_and_visible_for_resume(self):
        with self.assertRaisesRegex(target.ValidationError, 'Missing 2'):
            self.run_rows([self.row()], False)
        report = self.run_rows([self.row()])
        self.assertFalse(report['accepted_complete_run'])
        self.assertEqual(report['summary']['accuracy'], 1 / 3)
        self.assertEqual(len(report['missing_ids']), 2)

    def test_duplicate_and_unknown_ids(self):
        for rows in [[self.row(), self.row()], [self.row('unknown')]]:
            with self.subTest(rows=rows), self.assertRaises(target.ValidationError):
                self.run_rows(rows)

    def test_only_boolean_labels(self):
        for label in [0, 1, 'yes', 'false', None, [], {}]:
            with self.subTest(label=label), self.assertRaisesRegex(target.ValidationError, 'boolean'):
                self.run_rows([self.row(correct=label)])

    def test_official_envelope(self):
        row = self.row()
        del row['correct']
        row['autoeval_label'] = {'model': 'synthetic', 'label': False}
        self.assertEqual(self.run_rows([row])['summary']['correct'], 0)
        row['correct'] = False
        with self.assertRaisesRegex(target.ValidationError, 'ambiguous'):
            self.run_rows([row])

    def test_manifest_and_configuration_changes(self):
        with self.assertRaisesRegex(target.ValidationError, 'hash mismatch'):
            self.run_rows([self.row(manifest_sha256='wrong')])
        self.write('config.json', {**self.settings, 'reader_model': 'changed-reader'})
        with self.assertRaisesRegex(target.ValidationError, 'Manifest mismatch'):
            self.run_rows([self.row()])

    def test_dataset_bytes_bound_even_when_ids_same(self):
        self.dataset.write_text(self.dataset.read_text() + '\n')
        with self.assertRaisesRegex(target.ValidationError, 'Manifest mismatch'):
            self.run_rows([self.row()])

    def test_category_and_failed_record_contract(self):
        cases = [self.row(question_type='wrong'), self.row(status='other'),
                 self.row(status='failed', failure_stage='ingest')]
        failed = self.row(status='failed')
        del failed['correct']
        cases.append(failed)
        for row in cases:
            with self.subTest(row=row), self.assertRaises(target.ValidationError):
                self.run_rows([row])

    def test_malformed_duplicate_keys_and_blank_records(self):
        for text in ['{', '{}{}\n', '{"x":1,"x":2}\n', 'NaN\n', '\n']:
            self.results.write_text(text)
            with self.subTest(text=text), self.assertRaises(target.ValidationError):
                target.validate(self.dataset, self.config, self.manifest, self.results, True)

    def test_duplicate_dataset_ids(self):
        self.write('dataset.json', [{'question_id': 'x', 'question_type': 't'}] * 2)
        with self.assertRaisesRegex(target.ValidationError, 'Duplicate dataset'):
            target.build_manifest(self.dataset, self.config)

    def test_empty_resume_is_not_complete(self):
        report = self.run_rows([])
        self.assertEqual(report['summary']['accuracy'], 0)
        self.assertFalse(report['accepted_complete_run'])

    def test_cli_never_overwrites_manifest(self):
        before = self.manifest.read_bytes()
        with contextlib.redirect_stderr(io.StringIO()):
            code = target.main(['manifest', '--dataset', str(self.dataset), '--config',
                                str(self.config), '--manifest', str(self.manifest)])
        self.assertEqual(code, 2)
        self.assertEqual(before, self.manifest.read_bytes())

    def test_every_required_config_field(self):
        for field in self.settings:
            settings = dict(self.settings)
            del settings[field]
            self.write('config.json', settings)
            with self.subTest(field=field), self.assertRaisesRegex(target.ValidationError, field):
                target.build_manifest(self.dataset, self.config)

    def test_invalid_config_types_and_values(self):
        cases = [('context_budget_tokens', value) for value in (0, -1, True, 1.5, '2000')]
        cases += [('seed', value) for value in (None, True, '17', 1.5)]
        cases += [(field, value) for field in ('reader_model', 'judge_model', 'embedding_model')
                  for value in ('', '   ', None, 123)]
        cases += [(field, value) for field in ('engine_sha', 'harness_sha',
                  'reader_prompt_sha256', 'judge_prompt_sha256', 'execution_config_sha256')
                  for value in ('abc', 'g' * 64, 'A' * 64, None)]
        for field, value in cases:
            self.write('config.json', {**self.settings, field: value})
            with self.subTest(field=field, value=value), self.assertRaises(target.ValidationError):
                target.build_manifest(self.dataset, self.config)

    def test_empty_denominator_and_no_completed_accuracy(self):
        self.assertIsNone(target.counts([], {})['accuracy'])
        self.assertIsNone(target.counts([], {})['completed_qa_accuracy'])
        report = target.counts(['a'], {'a': None})
        self.assertEqual(report['accuracy'], 0)
        self.assertIsNone(report['completed_qa_accuracy'])
        report = target.counts(['a', 'b'], {'a': True, 'b': None})
        self.assertEqual(report['accuracy'], 0.5)
        self.assertEqual(report['completed_qa_accuracy'], 1)

    def test_canonical_config_order_is_stable(self):
        original = target.build_manifest(self.dataset, self.config)
        config = json.loads(self.config.read_text())
        self.write('config.json', dict(reversed(list(config.items()))))
        self.assertEqual(original, target.build_manifest(self.dataset, self.config))


if __name__ == '__main__':
    unittest.main()
