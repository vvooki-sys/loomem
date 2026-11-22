import unittest

from ingest_receipt import (ReceiptError, expected_ids_from_mcp_result,
                            expected_ids_from_receipt)

SOURCE = '00000000-0000-4000-8000-000000000001'


def fixture():
    return {'version': 1, 'scope': 'source_and_fragments', 'status': 'complete',
            'source_id': SOURCE, 'fragment_ids': [SOURCE + ':f0', SOURCE + ':f3'],
            'fragments': {'stored': 2, 'failed': 0, 'skipped': 0,
                          'segmentation_status': 'complete'}}


class IngestReceiptTests(unittest.TestCase):
    def test_exact_sparse_ids_without_inventing_children(self):
        expected = frozenset((SOURCE, SOURCE + ':f0', SOURCE + ':f3'))
        self.assertEqual(expected_ids_from_receipt(fixture()), expected)
        self.assertEqual(expected_ids_from_mcp_result({
            'structuredContent': {'ingest_receipt': fixture()},
            'content': [{'text': 'Stored 999 fragments'}]}), expected)

    def test_empty_complete_manifest(self):
        data = fixture()
        data['fragment_ids'] = []
        data['fragments']['stored'] = 0
        self.assertEqual(expected_ids_from_receipt(data), frozenset([SOURCE]))

    def test_duplicate_foreign_and_malformed_children(self):
        for child in (SOURCE + ':f0', '00000000-0000-4000-8000-000000000002:f3',
                      SOURCE, SOURCE + ':f01', SOURCE + ':f-1', SOURCE + ':f3\n',
                      None, {}, 3):
            data = fixture()
            data['fragment_ids'][1] = child
            with self.subTest(child=child), self.assertRaises(ReceiptError):
                expected_ids_from_receipt(data)

    def test_version_scope_status_and_source(self):
        for key, values in {'version': [True, 0, 2, '1'], 'scope': ['all', None],
                            'status': ['partial', None],
                            'source_id': [None, '', 'old-source', SOURCE.upper() + 'x']}.items():
            for value in values:
                data = fixture()
                data[key] = value
                with self.subTest(key=key, value=value), self.assertRaises(ReceiptError):
                    expected_ids_from_receipt(data)

    def test_counters_and_segmentation(self):
        for key, values in {'stored': [True, -1, 1, 3, 2.0, '2'],
                            'failed': [True, -1, 1], 'skipped': [True, -1, 1],
                            'segmentation_status': ['disabled', 'not_segmented', None]}.items():
            for value in values:
                data = fixture()
                data['fragments'][key] = value
                with self.subTest(key=key, value=value), self.assertRaises(ReceiptError):
                    expected_ids_from_receipt(data)

    def test_missing_or_extra_receipt_fields(self):
        for nested in (False, True):
            data = fixture()
            target = data['fragments'] if nested else data
            for key in list(target):
                removed = target.pop(key)
                with self.assertRaises(ReceiptError):
                    expected_ids_from_receipt(data)
                target[key] = removed
            target['unknown'] = 1
            with self.assertRaises(ReceiptError):
                expected_ids_from_receipt(data)

    def test_old_or_error_mcp_results_fail_closed(self):
        for value in (None, [], {}, {'content': [{'text': 'Stored 2 fragments'}]},
                      {'structuredContent': None},
                      {'isError': True, 'structuredContent': {'ingest_receipt': fixture()}}):
            with self.subTest(value=value), self.assertRaises(ReceiptError):
                expected_ids_from_mcp_result(value)


if __name__ == '__main__':
    unittest.main()
