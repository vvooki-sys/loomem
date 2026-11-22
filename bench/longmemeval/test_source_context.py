import unittest
from source_context import grounded, windows, select

class SourceContextTests(unittest.TestCase):
    def test_rejects_invented_time_without_changing_original(self):
        source = 'user: I just got back from the festival.'
        self.assertFalse(grounded('Visited three weeks ago.', None, source))
        self.assertTrue(grounded(source, None, source))
        self.assertFalse(grounded('Visited.', 'yesterday', source))
        self.assertTrue(grounded('Visited two weeks ago.', 'two weeks ago', 'Visited TWO  weeks ago.'))

    def test_original_offsets_cover_utf8_and_tail(self):
        source = {'id':'s','content':'user: ' + 'Zażółć ' * 600 + 'silver lantern'}
        parts = list(windows(source))
        self.assertTrue(any('silver lantern' in text for _, _, text in parts))
        for a, b, text in parts:
            self.assertEqual(source['content'][a:b], text)
            self.assertEqual(source['content'].encode()[len(source['content'][:a].encode()):len(source['content'][:b].encode())].decode(), text)

    def test_query_finds_original_even_without_extracted_facts(self):
        sources = [{'id':'a','content':'user: The silver lantern is under my bed.'},
                   {'id':'b','content':'assistant: Generic advice about lamps.'}]
        ranked = select('Where is my silver lantern?', sources, [])
        self.assertEqual(ranked[0][1], 'a')
        self.assertIn('under my bed', ranked[0][4])
