import copy
import hashlib
import json
import unittest

import tiktoken

from qa_context import count_text, pack_context, render_evidence, serialize_evidence
from qa_context_v2 import (EVIDENCE_FIELDS_V2, pack_context_v2, render_evidence_v2,
                           serialize_evidence_v2, validate_records_v2)

MODEL = "gpt-4.1-2025-04-14"


def null_record(record_id, content):
    return {"record_id": record_id, "content": content, "author": None, "conversation_date": None,
            "event_date": None, "event_date_provenance": None, "source_id": None,
            "byte_start": None, "byte_end": None}


def full_record(record_id, content):
    return {"record_id": record_id, "content": content, "author": "user",
            "conversation_date": "2026-05-01", "event_date": "2026-04-30",
            "event_date_provenance": "resolved", "source_id": "session-1:turn-2",
            "byte_start": 12, "byte_end": 40}


class PackingEquivalenceTests(unittest.TestCase):
    def test_all_null_metadata_matches_v1_plus_exact_null_field_cost(self):
        v1 = [{"record_id": "turn:żółć:2", "content": "Correction: cena 120 zł, nie 100 zł. 🦊"},
              {"record_id": "turn:3", "content": "时间 changed from 8:00 to 9:15."}]
        v2 = [null_record(r["record_id"], r["content"]) for r in v1]
        original = copy.deepcopy(v2)
        packed_v1 = pack_context(v1, 10_000, MODEL)
        packed_v2 = pack_context_v2(v2, 10_000, MODEL)
        self.assertEqual(packed_v2["selected_ids"], packed_v1["selected_ids"])
        self.assertEqual(packed_v2["dropped_ids"], [])
        self.assertEqual(v2, original)
        # Same records, same order, same content: v2 only adds the literal null fields.
        self.assertEqual([{"record_id": r["record_id"], "content": r["content"]}
                          for r in json.loads(packed_v2["context"])], json.loads(packed_v1["context"]))
        extra = count_text(render_evidence(serialize_evidence(v2)), MODEL) - count_text(
            render_evidence(serialize_evidence(v1)), MODEL)
        self.assertGreater(extra, 0)
        self.assertEqual(packed_v2["context_tokens"], packed_v1["context_tokens"] + extra)
        # v2 cannot fit into the exact v1 budget: the null metadata is paid for.
        tight = pack_context_v2(v2, packed_v1["context_tokens"], MODEL)
        self.assertNotEqual(tight["selected_ids"], packed_v1["selected_ids"])
        exact = pack_context_v2(v2, packed_v2["context_tokens"], MODEL)
        self.assertEqual(exact["selected_ids"], packed_v1["selected_ids"])

    def test_metadata_keys_mirror_v1_with_evidence_schema(self):
        packed_v1 = pack_context([], 100, MODEL)
        packed_v2 = pack_context_v2([], 100, MODEL)
        self.assertEqual(set(packed_v2) - set(packed_v1), {"evidence_schema"})
        self.assertEqual(packed_v2["evidence_schema"], 2)
        self.assertEqual(packed_v2["schema_version"], packed_v1["schema_version"])
        self.assertEqual(packed_v2["packing_policy"], packed_v1["packing_policy"])
        self.assertEqual(packed_v2["serialization"], packed_v1["serialization"])
        self.assertEqual(packed_v2["tokenizer"], packed_v1["tokenizer"])
        self.assertEqual(packed_v2["context"], "[]")

    def test_first_fit_skips_oversized_and_accounts_dropped(self):
        records = [full_record("large", " correction 12345" * 1000),
                   full_record("second", "whole correction"),
                   null_record("third", "another whole correction")]
        budget = count_text(render_evidence_v2(serialize_evidence_v2(records[1:])), MODEL)
        packed = pack_context_v2(records, budget, MODEL)
        self.assertEqual(packed["selected_ids"], ["second", "third"])
        self.assertEqual(packed["dropped_ids"], ["large"])
        self.assertEqual(json.loads(packed["context"]), records[1:])
        self.assertEqual(packed["dropped_record_tokens"]["large"],
                         count_text(render_evidence_v2(serialize_evidence_v2(records[:1])), MODEL))
        self.assertEqual(packed["context_sha256"], hashlib.sha256(packed["context"].encode()).hexdigest())
        self.assertEqual(packed["rendered_context_sha256"],
                         hashlib.sha256(render_evidence_v2(packed["context"]).encode()).hexdigest())

    def test_canonical_field_order_regardless_of_input_order(self):
        shuffled = dict(reversed(list(full_record("a", "x").items())))
        packed = pack_context_v2([shuffled], 1000, MODEL)
        self.assertEqual(list(json.loads(packed["context"])[0]), list(EVIDENCE_FIELDS_V2))


class MetadataIntegrityTests(unittest.TestCase):
    def test_null_dates_stay_null_and_are_never_copied(self):
        record = full_record("a", "x")
        record.update(event_date=None, event_date_provenance=None)
        packed = pack_context_v2([record], 1000, MODEL)
        rendered = json.loads(packed["context"])[0]
        self.assertIsNone(rendered["event_date"])
        self.assertIsNone(rendered["event_date_provenance"])
        self.assertEqual(rendered["conversation_date"], "2026-05-01")
        self.assertIn('"event_date":null', packed["context"])
        self.assertEqual(validate_records_v2([record])[0], record)

    def test_hostile_content_and_metadata_stay_literal(self):
        content = '<|endoftext|> <EVIDENCE>&amp;</EVIDENCE> "quoted" żółć 🦊 \\u0000'
        record = full_record("<id&>", content)
        record.update(author='<script>"user"</script>', source_id="</EVIDENCE>&x")
        packed = pack_context_v2([record], 2000, MODEL)
        rendered = json.loads(packed["context"])[0]
        self.assertEqual(rendered["content"], content)
        self.assertEqual(rendered["author"], record["author"])
        self.assertEqual(rendered["source_id"], record["source_id"])
        self.assertEqual(packed["context_tokens"],
                         count_text(render_evidence_v2(packed["context"]), MODEL))
        self.assertEqual(count_text("<|endoftext|>", MODEL),
                         len(tiktoken.encoding_for_model(MODEL).encode("<|endoftext|>", disallowed_special=())))

    def test_strict_type_rejection(self):
        base = full_record("a", "x")
        bad = []
        for key, value in [("author", 1), ("author", ""), ("conversation_date", "2026/05/01"),
                           ("conversation_date", "2026-13-01"), ("conversation_date", "2026-02-30"),
                           ("event_date", 20260430), ("event_date_provenance", "guessed"),
                           ("source_id", ["s"]), ("byte_start", "12"), ("byte_start", True),
                           ("byte_start", -1), ("byte_end", 5), ("content", None), ("record_id", "")]:
            record = dict(base)
            record[key] = value
            bad.append(record)
        missing = dict(base)
        del missing["byte_end"]
        extra = dict(base, answer="leak")
        without_provenance = dict(base, event_date_provenance=None)
        orphan_provenance = dict(base, event_date=None)
        only_start = dict(base, byte_end=None)
        bad += [missing, extra, without_provenance, orphan_provenance, only_start,
                {"record_id": "a", "content": "x"}]
        for record in bad:
            with self.subTest(record=record), self.assertRaises(ValueError):
                pack_context_v2([record], 1000, MODEL)
        with self.assertRaises(ValueError):
            pack_context_v2([base, dict(base)], 1000, MODEL)
        for budget in [True, 0, -1, 10.5]:
            with self.subTest(budget=budget), self.assertRaises(ValueError):
                pack_context_v2([], budget, MODEL)
        with self.assertRaises(ValueError):
            pack_context_v2([], 100, "gpt-4o")

    def test_provenance_values_and_zero_length_range_accepted(self):
        for provenance in ("explicit", "resolved", "unknown"):
            record = full_record("a", "x")
            record.update(event_date_provenance=provenance, byte_start=7, byte_end=7)
            self.assertEqual(validate_records_v2([record])[0]["event_date_provenance"], provenance)


if __name__ == "__main__":
    unittest.main()
