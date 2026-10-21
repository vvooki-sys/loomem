import io
import json
from types import SimpleNamespace
import unittest

import runner
import runner_local_v2
from runner_local_v2 import (LocalEngineAdapterV2, failed_trace_row_v2, map_rejections,
                             trace_row_v2, write_trace_row_v2)
import stage_diagnostics

STAGES = ("ranked_candidates", "after_soft_delete", "after_filters_and_dedup", "response")


def engine_response(results, stages, rejections):
    return {"results": results,
            "trace_metadata": {
                "evidence": {"schema_version": 1, "coverage": "post_rank_to_response",
                             "stages": [{"stage": name, "chunk_ids": ids}
                                        for name, ids in zip(STAGES, stages)]},
                "rejections": rejections}}


class RejectionMappingTests(unittest.TestCase):
    def adapter(self):
        adapter = LocalEngineAdapterV2.__new__(LocalEngineAdapterV2)
        adapter.settings = {"capture_context": False}
        adapter.process = SimpleNamespace(poll=lambda: None)
        adapter.mapping = {"engine-a": "record-a", "engine-b": "record-b", "engine-c": "record-c"}
        return adapter

    def search(self, adapter, response):
        adapter.engine_request = lambda path, payload=None: response
        return adapter.request("/search", {"query": "q", "top_k": 2, "trace": True})

    def test_rejections_are_mapped_to_record_ids_and_unmapped_ids_dropped(self):
        adapter = self.adapter()
        response = engine_response(
            [{"id": "engine-a", "content": "A"}],
            [["engine-a", "engine-b", "engine-c", "engine-zz"], ["engine-a", "engine-b"],
             ["engine-a"], ["engine-a"]],
            [{"chunk_id": "engine-c", "stage": "after_soft_delete", "reason": "soft_deleted"},
             {"chunk_id": "engine-zz", "stage": "after_soft_delete", "reason": "soft_deleted"},
             {"chunk_id": "engine-b", "stage": "after_filters_and_dedup",
              "reason": "content_prefix_dedup"}])
        # A stage id outside the mapping is a hard failure, as in v1.
        with self.assertRaises(KeyError):
            self.search(adapter, response)
        response["trace_metadata"]["evidence"]["stages"][0]["chunk_ids"].remove("engine-zz")
        result = self.search(adapter, response)
        self.assertEqual(result["retrieved_ids"], ["record-a"])
        self.assertEqual(result["rejections"], [
            {"chunk_id": "record-c", "stage": "after_soft_delete", "reason": "soft_deleted"},
            {"chunk_id": "record-b", "stage": "after_filters_and_dedup",
             "reason": "content_prefix_dedup"}])
        # Top-level sibling of retrieved_ids/trace_metadata; the trace itself is
        # untouched apart from the v1 stage id mapping.
        self.assertEqual(set(result), {"retrieved_ids", "trace_metadata", "rejections"})
        self.assertEqual(result["trace_metadata"]["evidence"]["stages"][0]["chunk_ids"],
                         ["record-a", "record-b", "record-c"])
        # The frozen v1 projection still strips rejections from `evidence`.
        self.assertEqual(set(runner.trace_projection(result)), {"coverage", "stages"})

    def test_missing_or_empty_rejections_yield_empty_list(self):
        adapter = self.adapter()

        def response():
            return engine_response([{"id": "engine-a", "content": "A"}], [["engine-a"]] * 4, [])
        self.assertEqual(self.search(adapter, response())["rejections"], [])
        without = response()
        del without["trace_metadata"]["rejections"]
        self.assertEqual(self.search(adapter, without)["rejections"], [])
        self.assertEqual(self.search(adapter, {"results": []})["rejections"], [])

    def test_malformed_rejections_fail_the_search(self):
        for bad in [{"chunk_id": "engine-a", "stage": "response"},
                    {"chunk_id": "engine-a", "stage": "response", "reason": 3},
                    {"chunk_id": "engine-a", "stage": "", "reason": "truncated_top_k"},
                    {"chunk_id": "engine-a", "stage": "response", "reason": "x", "extra": 1},
                    "engine-a"]:
            with self.subTest(bad=bad), self.assertRaises(ValueError):
                self.search(self.adapter(), engine_response(
                    [{"id": "engine-a", "content": "A"}], [["engine-a"]] * 4, [bad]))
        with self.assertRaises(ValueError):
            map_rejections({"trace_metadata": {"rejections": {}}}, lambda _: None)

    def test_capture_context_result_also_carries_rejections(self):
        adapter = self.adapter()
        adapter.settings["capture_context"] = True
        adapter.engine_request = lambda path, payload=None: engine_response(
            [{"id": "engine-a", "content": "A"}], [["engine-a", "engine-b"]] + [["engine-a"]] * 3,
            [{"chunk_id": "engine-b", "stage": "after_soft_delete", "reason": "soft_deleted"}])
        result = adapter.request("/search", {"query": "q", "top_k": 1, "trace": True,
                                             "capture_context": True})
        self.assertEqual(result["rejections"][0]["chunk_id"], "record-b")
        self.assertEqual(result["retrieved_records"], [{"record_id": "record-a", "content": "A"}])
        self.assertEqual(result["retrieved_records_v2"][0]["record_id"], "record-a")


class SidecarTests(unittest.TestCase):
    def found(self):
        return {"retrieved_ids": ["record-a"],
                "trace_metadata": {"evidence": {
                    "schema_version": 1, "coverage": "post_rank_to_response",
                    "stages": [{"stage": name, "chunk_ids": ids} for name, ids in zip(
                        STAGES, [["record-a", "record-b", "record-c"], ["record-a", "record-b"],
                                 ["record-a"], ["record-a"]])]},
                    "rejections": [{"chunk_id": "engine-raw", "stage": "x", "reason": "y"}]},
                "rejections": [
                    {"chunk_id": "record-c", "stage": "after_soft_delete", "reason": "soft_deleted"},
                    {"chunk_id": "record-b", "stage": "after_filters_and_dedup",
                     "reason": "content_prefix_dedup"}]}

    def test_row_carries_v1_projection_and_top_level_rejections(self):
        row = trace_row_v2(self.found(), {"question_id": "q1"}, "m" * 64)
        self.assertEqual(set(row), {"question_id", "manifest_sha256", "status", "retrieved_ids",
                                    "evidence", "rejections"})
        self.assertEqual(row["evidence"], runner.trace_projection(self.found()))
        self.assertEqual([item["chunk_id"] for item in row["rejections"]], ["record-c", "record-b"])
        self.assertNotIn("engine-raw", json.dumps(row))
        self.assertEqual(failed_trace_row_v2({"question_id": "q2"}, "m" * 64, "ingest"), {
            "question_id": "q2", "manifest_sha256": "m" * 64, "status": "failed",
            "failure_stage": "ingest", "rejections": []})
        with self.assertRaises(ValueError):
            trace_row_v2(dict(self.found(), rejections=[{"chunk_id": "a"}]), {"question_id": "q"}, "m")

    def test_written_rows_feed_stage_diagnostics_end_to_end(self):
        handle = io.StringIO()
        write_trace_row_v2(handle, trace_row_v2(self.found(), {"question_id": "q1"}, "m" * 64))
        write_trace_row_v2(handle, failed_trace_row_v2({"question_id": "q2"}, "m" * 64, "ingest"))
        rows = [json.loads(line) for line in handle.getvalue().splitlines()]
        report = stage_diagnostics.diagnose(rows, {"q1": ["record-b", "record-c"]})
        q1 = report["questions"][0]
        self.assertEqual(q1["rejections_by_reason"], {"content_prefix_dedup": 1, "soft_deleted": 1})
        self.assertEqual([item["chunk_id"] for item in q1["relevant_rejections"]], ["record-c", "record-b"])
        self.assertEqual(q1["relevant"]["record-b"]["first_absent"], "after_filters_and_dedup")
        self.assertEqual(q1["relevant"]["record-c"]["first_absent"], "after_soft_delete")
        self.assertEqual(q1["stage_counts"]["ranked_candidates"], 3)
        self.assertEqual(report["aggregate"]["rejections_by_reason"],
                         {"content_prefix_dedup": 1, "soft_deleted": 1})
        self.assertEqual(report["questions"][1]["status"], "failed")
        self.assertEqual(report["questions"][1]["stage_counts"]["response"], None)


if __name__ == "__main__":
    unittest.main()
