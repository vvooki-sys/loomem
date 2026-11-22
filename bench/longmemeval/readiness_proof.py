"""Strict evidence validation for a fresh, sealed local evaluation engine."""
from validate import fail

EVIDENCE = "fresh_immutable_source_fragments_v1"
STATES = {
    "chunk": {"active", "unavailable", "content_mismatch", "metadata_mismatch"},
    "vector": {"valid", "missing", "invalid", "unavailable"},
    "document": {"current", "missing", "duplicate", "content_mismatch"},
}


def validate_proof(proof, scope, expected_ids):
    """Return proven readiness, rejecting inconsistent or incomplete evidence."""
    fields = {"schema_version", "scope", "evidence", "dimension", "ready", "records"}
    if not isinstance(proof, dict) or set(proof) != fields:
        fail("Invalid readiness proof fields")
    if (type(proof["schema_version"]) is not int or proof["schema_version"] != 1
            or proof["scope"] != scope or proof["evidence"] != EVIDENCE
            or type(proof["dimension"]) is not int or proof["dimension"] != 384
            or type(proof["ready"]) is not bool or not isinstance(proof["records"], list)):
        fail("Invalid readiness proof identity")
    seen, complete = set(), True
    for record in proof["records"]:
        if not isinstance(record, dict) or set(record) != {"id", *STATES}:
            fail("Invalid readiness record")
        record_id = record["id"]
        if not isinstance(record_id, str) or record_id in seen:
            fail("Invalid or duplicate readiness ID")
        seen.add(record_id)
        for key, states in STATES.items():
            if not isinstance(record[key], str) or record[key] not in states:
                fail("Invalid readiness state")
        complete &= (record["chunk"], record["vector"], record["document"]) == (
            "active", "valid", "current")
    if not expected_ids or seen != set(expected_ids):
        fail("Readiness IDs do not match the exact receipt set")
    if proof["ready"] != complete:
        fail("Readiness aggregate disagrees with record evidence")
    return complete
