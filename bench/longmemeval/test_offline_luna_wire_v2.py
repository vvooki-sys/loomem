import unittest

from offline_luna_wire_v2 import BODY_CAP, _canonical, adapt


class LunaWireTests(unittest.TestCase):
    def setUp(self):
        self.messages = [{'role': 'system', 'content': 'system'}, {'role': 'user', 'content': 'synthetic'}]
        self.legacy = {'model': 'gpt-6-luna', 'messages': self.messages, 'max_tokens': 2000, 'temperature': 0}
        self.native = {'model': 'gpt-6-luna', 'messages': self.messages,
                       'max_completion_tokens': 2000, 'reasoning_effort': 'low'}

    def test_native_rust_format_and_legacy_produce_same_wire(self):
        self.assertEqual(adapt(self.native), adapt(self.legacy))
        wire = adapt(self.native)
        self.assertNotIn('temperature', wire)
        self.assertEqual(wire['reasoning_effort'], 'low')
        self.messages[0]['content'] = 'changed'
        self.assertEqual(wire['messages'][0]['content'], 'system')

    def test_limits_and_exact_profile_are_fail_closed(self):
        for changes in ({'max_completion_tokens': 2048}, {'max_completion_tokens': True},
                        {'reasoning_effort': 'medium'}, {'model': 'gpt-4.1-mini'},
                        {'temperature': 0}, {'service_tier': 'default'}):
            with self.subTest(changes=changes), self.assertRaises(ValueError):
                adapt({**self.native, **changes})
        with self.assertRaises(ValueError):
            adapt({**self.legacy, 'model': 'gpt-4.1-mini'})

    def test_both_payload_caps_include_utf8_and_wire_overhead(self):
        for text in ('x' * BODY_CAP, 'ż' * (BODY_CAP // 2)):
            body = {**self.native, 'messages': [self.messages[0], {'role': 'user', 'content': text}]}
            self.assertGreater(len(_canonical(body)), BODY_CAP)
            with self.assertRaisesRegex(ValueError, 'body_cap'):
                adapt(body)
        overhead = len(_canonical(adapt(self.native))) - len(_canonical(self.native))
        self.assertGreater(overhead, 0)
        initial = len(_canonical(self.native))
        body = {**self.native, 'messages': [self.messages[0], {'role': 'user',
                'content': 'x' * (len(self.messages[1]['content']) + BODY_CAP - initial)}]}
        self.assertEqual(len(_canonical(body)), BODY_CAP)
        with self.assertRaisesRegex(ValueError, 'body_cap'):
            adapt(body)


if __name__ == '__main__':
    unittest.main()
