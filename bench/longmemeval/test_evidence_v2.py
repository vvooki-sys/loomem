import json
import unittest
from types import SimpleNamespace

from evidence_v2 import from_e0_turn_json, from_engine_hit, from_memory_item, null_metadata
from qa_context_v2 import EVIDENCE_FIELDS_V2
import runner
import runner_local_v2

EVIDENCE = {"author": "assistant", "conversation_date": "2026-05-01", "event_date": "2026-04-28",
            "event_date_provenance": "explicit", "source_id": "s1:t4", "byte_start": 0, "byte_end": 17}


class EngineHitTests(unittest.TestCase):
    def test_hit_without_evidence_maps_to_null_metadata(self):
        hit = {"id": "engine-1", "content": "Actual sanitized text", "score": 0.5, "metadata": None}
        record = from_engine_hit(hit, "record-1")
        self.assertEqual(record, {"record_id": "record-1", "content": "Actual sanitized text",
                                  **null_metadata()})
        self.assertEqual(tuple(record), EVIDENCE_FIELDS_V2)
        self.assertEqual(from_engine_hit(dict(hit, evidence=None), "record-1"), record)

    def test_hit_with_evidence_maps_every_field_verbatim(self):
        hit = {"id": "engine-1", "content": "text", "score": 1.0, "evidence": dict(EVIDENCE)}
        record = from_engine_hit(hit, "record-1")
        self.assertEqual(record, {"record_id": "record-1", "content": "text", **EVIDENCE})
        self.assertEqual(hit["evidence"], EVIDENCE)

    def test_event_date_never_filled_from_conversation_date(self):
        evidence = dict(EVIDENCE, event_date=None, event_date_provenance=None)
        record = from_engine_hit({"id": "e", "content": "t", "evidence": evidence}, "r")
        self.assertIsNone(record["event_date"])
        self.assertIsNone(record["event_date_provenance"])
        self.assertEqual(record["conversation_date"], "2026-05-01")

    def test_strict_rejection_of_unexpected_shapes(self):
        cases = [
            {"id": "e", "content": "t", "evidence": dict(EVIDENCE, extra=1)},
            {"id": "e", "content": "t", "evidence": {k: v for k, v in EVIDENCE.items() if k != "author"}},
            {"id": "e", "content": "t", "evidence": []},
            {"id": "e", "content": "t", "evidence": dict(EVIDENCE, byte_start="0")},
            {"id": "e", "content": "t", "evidence": dict(EVIDENCE, conversation_date="May 1, 2026")},
            {"id": "e", "content": "t", "evidence": dict(EVIDENCE, event_date_provenance=None)},
            {"id": "e", "content": "t", "evidence": dict(EVIDENCE, author=["user"])},
            {"id": "e", "content": {"text": "t"}},
            {"id": 7, "content": "t"},
            {"content": "t"},
            ["id", "content"],
        ]
        for hit in cases:
            with self.subTest(hit=hit), self.assertRaises(ValueError):
                from_engine_hit(hit, "r")
        with self.assertRaises(ValueError):
            from_engine_hit({"id": "e", "content": "t"}, "")

    def test_memory_item_does_not_lift_unprovenanced_event_date(self):
        item = {"id": "e", "content": "stored", "event_date": "2026-01-01", "created_at": 1,
                "source_agent": "bench", "source_stream": "s"}
        record = from_memory_item(item, "r")
        self.assertEqual(record, {"record_id": "r", "content": "stored", **null_metadata()})
        with_evidence = from_memory_item(dict(item, evidence=dict(EVIDENCE)), "r")
        self.assertEqual(with_evidence, {"record_id": "r", "content": "stored", **EVIDENCE})
        with self.assertRaises(ValueError):
            from_memory_item(dict(item, content=None), "r")


class E0EnvelopeTests(unittest.TestCase):
    def test_exact_envelope_is_parsed_into_metadata(self):
        stored = json.dumps({"role": "user", "date": "2026-05-01", "content": "Inner <text> & 🦊"})
        record = from_e0_turn_json(stored, "r")
        self.assertEqual(record["author"], "user")
        self.assertEqual(record["conversation_date"], "2026-05-01")
        self.assertEqual(record["content"], "Inner <text> & 🦊")
        for field in ("event_date", "event_date_provenance", "source_id", "byte_start", "byte_end"):
            self.assertIsNone(record[field])

    def test_non_envelope_strings_stay_verbatim_with_null_metadata(self):
        for text in ["plain turn text", "{not json", "[]", "\"string\"", "42",
                     json.dumps({"role": "user", "content": "missing date"}),
                     json.dumps({"role": "user", "date": "2026-05-01", "content": "x", "extra": 1}),
                     json.dumps({"role": "user", "date": 20260501, "content": "x"}),
                     json.dumps({"role": "user", "date": "2026-05-01", "content": "x"}) + " ",
                     ""]:
            with self.subTest(text=text):
                record = from_e0_turn_json(text, "r")
                self.assertEqual(record["content"], text)
                self.assertEqual({k: record[k] for k in null_metadata()}, null_metadata())

    def test_envelope_dates_are_never_rewritten(self):
        stored = json.dumps({"role": "user", "date": "2023/05/20 (Sat) 02:30", "content": "x"})
        with self.assertRaises(ValueError):
            from_e0_turn_json(stored, "r")
        with self.assertRaises(ValueError):
            from_e0_turn_json(None, "r")


class LocalAdapterV2Tests(unittest.TestCase):
    def adapter(self):
        adapter = runner_local_v2.LocalEngineAdapterV2.__new__(runner_local_v2.LocalEngineAdapterV2)
        adapter.settings = {"capture_context": True}
        adapter.process = SimpleNamespace(poll=lambda: None)
        adapter.mapping = {"engine-a": "record-a", "engine-b": "record-b"}
        return adapter

    def test_search_capture_keeps_v1_records_and_adds_v2(self):
        adapter = self.adapter()
        forwarded = []

        def search_request(path, payload=None):
            forwarded.append((path, payload))
            return {"results": [
                {"id": "engine-b", "content": "Actual B", "evidence": dict(EVIDENCE)},
                {"id": "engine-a", "content": "Actual A"}]}
        adapter.engine_request = search_request
        result = adapter.request("/search", {"query": "q", "top_k": 2, "trace": True,
                                             "capture_context": True})
        v1 = [{"record_id": "record-b", "content": "Actual B"},
              {"record_id": "record-a", "content": "Actual A"}]
        self.assertEqual(result["retrieved_records"], v1)
        self.assertEqual(result["retrieved_ids"], ["record-b", "record-a"])
        self.assertEqual(result["retrieved_records_v2"], [
            {"record_id": "record-b", "content": "Actual B", **EVIDENCE},
            {"record_id": "record-a", "content": "Actual A", **null_metadata()}])
        self.assertEqual(runner.context_records(result["retrieved_records"], result["retrieved_ids"]), v1)
        self.assertEqual(runner_local_v2.context_records_v2(result["retrieved_records_v2"],
                                                            result["retrieved_ids"]),
                         result["retrieved_records_v2"])
        self.assertTrue(all(payload["dry_run"] is True for _, payload in forwarded))
        plain = adapter.request("/search", {"query": "q", "top_k": 2, "trace": True})
        self.assertNotIn("retrieved_records", plain)
        self.assertNotIn("retrieved_records_v2", plain)

    def test_malformed_engine_evidence_fails_the_search(self):
        adapter = self.adapter()
        adapter.engine_request = lambda path, payload=None: {"results": [
            {"id": "engine-a", "content": "A", "evidence": {"author": "user"}}]}
        with self.assertRaises(ValueError):
            adapter.request("/search", {"query": "q", "top_k": 1, "trace": True,
                                        "capture_context": True})

    def test_fetch_records_v2_is_scoped_ordered_and_null_without_evidence(self):
        adapter = self.adapter()
        requests = []

        def request(path, payload=None):
            requests.append(path)
            engine_id = path.rsplit("/", 1)[1]
            return {"id": engine_id, "content": "Stored " + engine_id, "event_date": "2026-01-01"}
        adapter.engine_request = request
        self.assertEqual(adapter.fetch_records_v2(["record-b", "record-a"]), [
            {"record_id": "record-b", "content": "Stored engine-b", **null_metadata()},
            {"record_id": "record-a", "content": "Stored engine-a", **null_metadata()}])
        self.assertEqual(requests, ["/api/memories/engine-b", "/api/memories/engine-a"])
        with self.assertRaises(ValueError):
            adapter.fetch_records_v2(["another-question"])
        adapter.settings["capture_context"] = False
        with self.assertRaises(ValueError):
            adapter.fetch_records_v2(["record-a"])

    def test_context_records_v2_rejects_v1_shape_and_wrong_order(self):
        with self.assertRaises(ValueError):
            runner_local_v2.context_records_v2([{"record_id": "a", "content": "x"}], ["a"])
        records = [{"record_id": "a", "content": "x", **null_metadata()}]
        with self.assertRaises(ValueError):
            runner_local_v2.context_records_v2(records, ["b"])
        artifact = runner_local_v2.context_artifact_v2(
            {"retrieved_records_v2": records}, {"question_id": "q"}, "m" * 64, ["a"])
        self.assertEqual(artifact["schema_version"], 2)
        self.assertEqual(artifact["records"], records)


if __name__ == "__main__":
    unittest.main()
