"""Reject plausible but incomplete, stale, or internally inconsistent proofs."""
import copy
import unittest

from readiness_proof import EVIDENCE, validate_proof
from validate import ValidationError


def valid():
    return {"schema_version": 1, "scope": "question-scope", "evidence": EVIDENCE,
            "dimension": 384, "ready": True, "records": [
                {"id": "source", "chunk": "active", "vector": "valid", "document": "current"},
                {"id": "source:f7", "chunk": "active", "vector": "valid", "document": "current"}]}


class ProofTests(unittest.TestCase):
    def validate(self, value):
        return validate_proof(value, "question-scope", {"source", "source:f7"})

    def test_complete_and_pending(self):
        self.assertTrue(self.validate(valid()))
        for key, value in (("chunk", "metadata_mismatch"), ("vector", "missing"),
                           ("document", "duplicate")):
            item = valid()
            item["records"][1][key] = value
            item["ready"] = False
            self.assertFalse(self.validate(item))

    def test_identity_and_schema(self):
        for key, value in (("schema_version", True), ("schema_version", 2),
                           ("dimension", 1536), ("dimension", 384.0), ("scope", "other"),
                           ("evidence", "count_only"), ("ready", 1), ("records", {})):
            item = valid()
            item[key] = value
            with self.subTest(key=key, value=value), self.assertRaises(ValidationError):
                self.validate(item)
        for item in (None, {}, dict(valid(), unexpected=1)):
            with self.assertRaises(ValidationError):
                self.validate(item)

    def test_missing_extra_duplicate_and_malformed_record(self):
        cases = []
        item = valid()
        item["records"].pop()
        cases.append(item)
        item = valid()
        item["records"].append(dict(item["records"][0], id="orphan"))
        cases.append(item)
        item = valid()
        item["records"][1] = copy.deepcopy(item["records"][0])
        cases.append(item)
        for record in (None, {"id": "source"}, dict(valid()["records"][0], extra=1),
                       dict(valid()["records"][0], vector="unknown"),
                       dict(valid()["records"][0], id=[])):
            item = valid()
            item["records"][0] = record
            cases.append(item)
        for item in cases:
            with self.subTest(item=item), self.assertRaises(ValidationError):
                self.validate(item)

    def test_aggregate_must_equal_states(self):
        item = valid()
        item["ready"] = False
        with self.assertRaises(ValidationError):
            self.validate(item)
        for key, state in (("chunk", "unavailable"), ("vector", "invalid"),
                           ("document", "content_mismatch")):
            item = valid()
            item["records"][0][key] = state
            with self.subTest(key=key), self.assertRaises(ValidationError):
                self.validate(item)
