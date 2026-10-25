"""Pure smoke guard tests; these do not claim a real-engine integration run."""
import socket
import unittest
from unittest.mock import patch

from runner_local_e1 import render_transcript
from smoke_local_e1 import loopback_only, workload, assert_wire_trace


class SmokeGuardTests(unittest.TestCase):
    def test_synthetic_workload_has_late_information_and_no_evaluation_labels(self):
        row = workload()
        self.assertNotIn('answer', row)
        text = render_transcript(row['haystack_sessions'][0])
        self.assertGreater(text.index('café-orbit'), 4000)
        self.assertEqual(row['haystack_sessions'][0][-2]['role'], 'user')
        self.assertTrue(all(set(turn) == {'role', 'content'}
                            for turn in row['haystack_sessions'][0]))

    def test_parent_guard_blocks_remote_tokenizer_downloads_before_dns(self):
        with patch('socket.getaddrinfo') as resolver:
            with loopback_only():
                with self.assertRaises(OSError):
                    socket.getaddrinfo('example.invalid', 443)
                socket.getaddrinfo('127.0.0.1', 3030)
            resolver.assert_called_once_with('127.0.0.1', 3030)

    def test_wire_assertion_rejects_stale_response_order_and_false_rejection(self):
        stages = ['ranked_candidates', 'after_soft_delete', 'after_filters_and_dedup', 'response']
        wire = {'results': [{'id': 'a'}, {'id': 'b'}], 'trace_metadata': {
            'evidence': {'schema_version': 1, 'coverage': 'post_rank_to_response',
                         'stages': [{'stage': stage, 'chunk_ids': ['a', 'b']} for stage in stages]},
            'rejections': []}}
        self.assertEqual(assert_wire_trace(wire), ['a', 'b'])
        wire['trace_metadata']['evidence']['stages'][-1]['chunk_ids'].reverse()
        with self.assertRaises(AssertionError):
            assert_wire_trace(wire)
        wire['trace_metadata']['evidence']['stages'][-1]['chunk_ids'].reverse()
        wire['trace_metadata']['rejections'] = [{'chunk_id': 'a'}]
        with self.assertRaises(AssertionError):
            assert_wire_trace(wire)


if __name__ == '__main__':
    unittest.main()
