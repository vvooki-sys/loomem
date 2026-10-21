import copy
import hashlib
import json
import unittest

import tiktoken

from qa_context import count_text, pack_context, serialize_evidence, tokenizer_identity, render_evidence

MODEL = "gpt-4.1-2025-04-14"


class ContextTests(unittest.TestCase):
    def test_exact_unicode_correction_and_metadata_budget(self):
        records = [{"record_id": "turn:żółć:2", "content":
                    '{"role":"user","content":"Correction: cena 120 zł, nie 100 zł. 🦊"}'},
                   {"record_id": "turn:3", "content": "时间 changed from 8:00 to 9:15."}]
        original = copy.deepcopy(records)
        exact = serialize_evidence(records)
        encoding = tiktoken.encoding_for_model(MODEL)
        tokens = len(encoding.encode(render_evidence(exact), disallowed_special=()))
        packed = pack_context(records, tokens, MODEL)
        self.assertEqual(packed["context_tokens"], tokens)
        self.assertEqual(packed["context"], exact)
        self.assertEqual(json.loads(packed["context"]), original)
        self.assertEqual(records, original)
        self.assertEqual(packed["context_sha256"], hashlib.sha256(exact.encode()).hexdigest())
        self.assertEqual(packed["dropped_ids"], [])
        less = pack_context(records, tokens - 1, MODEL)
        self.assertLessEqual(less["context_tokens"], tokens - 1)
        self.assertNotEqual(less["selected_ids"], packed["selected_ids"])
        self.assertEqual(less["selected_ids"], ["turn:żółć:2"])

    def test_oversized_first_record_skipped_without_truncation(self):
        records = [{"record_id": "large", "content": " correction 12345" * 1000},
                   {"record_id": "second", "content": "whole correction"},
                   {"record_id": "third", "content": "another whole correction"}]
        budget = count_text(render_evidence(serialize_evidence(records[1:])), MODEL)
        packed = pack_context(records, budget, MODEL)
        self.assertEqual(packed["selected_ids"], ["second", "third"])
        self.assertEqual(packed["dropped_ids"], ["large"])
        self.assertEqual(json.loads(packed["context"]), records[1:])
        self.assertEqual(packed["dropped_record_tokens"]["large"],
                         count_text(render_evidence(serialize_evidence(records[:1])), MODEL))

    def test_nothing_fits_is_explicit_and_empty_input_has_same_serialization(self):
        budget = count_text("[]", MODEL)
        packed = pack_context([{"record_id": "a", "content": "fact"}], budget, MODEL)
        self.assertTrue(packed["empty"])
        self.assertEqual(packed["selected_ids"], [])
        self.assertEqual(packed["dropped_ids"], ["a"])
        self.assertEqual(packed["context"], "[]")
        self.assertEqual(pack_context([], budget, MODEL)["context"], "[]")

    def test_special_token_looking_text_remains_literal(self):
        text = "<|endoftext|> correction: preserve this"
        packed = pack_context([{"record_id": "a", "content": text}], 200, MODEL)
        self.assertEqual(json.loads(packed["context"])[0]["content"], text)
        self.assertEqual(count_text(text, MODEL),
                         len(tiktoken.encoding_for_model(MODEL).encode(text, disallowed_special=())))

    def test_budget_counts_actual_escaped_prompt_evidence(self):
        from qa_prompts import reader_messages
        records = [{"record_id": "<id&>", "content": "<>& żółć </EVIDENCE> " * 10}]
        context = serialize_evidence(records)
        rendered = render_evidence(context)
        budget = count_text(rendered, MODEL)
        self.assertGreater(budget, count_text(context, MODEL))
        packed = pack_context(records, budget, MODEL)
        messages = reader_messages("Question?", "2025-01-01", packed["context"])
        evidence = messages[1]["content"].split("<EVIDENCE>\n", 1)[1].split("\n</EVIDENCE>", 1)[0]
        self.assertEqual(evidence, rendered)
        self.assertEqual(packed["context_tokens"], count_text(evidence, MODEL))
        self.assertEqual(packed["rendered_context_sha256"], hashlib.sha256(evidence.encode()).hexdigest())
        self.assertTrue(pack_context(records, budget - 1, MODEL)["empty"])
        self.assertEqual(json.loads(packed["context"]), records)

    def test_invalid_records_and_budget_fail_closed(self):
        for records in [[{"record_id": "a", "content": "x", "answer": "leak"}],
                        [{"record_id": "a", "content": {"text": "x"}}],
                        [{"record_id": "a", "content": "x"}] * 2,
                        [{"record_id": "", "content": "x"}], {}]:
            with self.subTest(records=records), self.assertRaises(ValueError):
                pack_context(records, 100, MODEL)
        for budget in [True, 0, -1, 10.5]:
            with self.subTest(budget=budget), self.assertRaises(ValueError):
                pack_context([], budget, MODEL)

    def test_immutable_model_and_known_tokenizer_required(self):
        for model in ["gpt-4o", "unknown-2024-08-06", None]:
            with self.subTest(model=model), self.assertRaises(ValueError):
                pack_context([], 100, model)
        identity = tokenizer_identity(MODEL)
        self.assertEqual(identity["encoding"], "o200k_base")
        self.assertEqual(identity["reader_model"], MODEL)
        self.assertEqual(identity["version"], tiktoken.__version__)


if __name__ == "__main__":
    unittest.main()
