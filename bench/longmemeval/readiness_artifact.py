"""Bounded historical diagnostics, not a live readiness lease or proof service."""
import json
from pathlib import Path

from readiness_proof import validate_proof
from validate import canonical_hash, fail

FINGERPRINTS = ("binary_sha256", "template_sha256", "model_onnx_sha256", "tokenizer_sha256")


def build_artifact(adapter, question_id, manifest_hash, stage, success, initialized):
    expected = adapter.expected_ids_by_source if initialized else {}
    mapping = adapter.mapping if initialized else {}
    sources = []
    for source_id, ids in sorted(expected.items()):
        sources.append({"source_id": source_id, "record_id": mapping[source_id],
                        "expected_ids": sorted(ids), "expected_ids_sha256": canonical_hash(sorted(ids)),
                        **adapter.readiness_observations.get(source_id, {
                            "attempts": 0, "last_proof": None, "failure": "not_probed"})})
    if success:
        if not sources:
            fail("Successful diagnostics require exact source proofs")
        scope = canonical_hash([manifest_hash, question_id])
        for source in sources:
            if source["failure"] is not None or not validate_proof(
                    source["last_proof"], scope, source["expected_ids"]):
                fail("Successful diagnostics lack a final source proof")
    config = adapter.settings.get("local_engine", {})
    exact = sorted(frozenset().union(*expected.values()))
    return {"schema_version": 1, "kind": "historical_readiness_diagnostics",
            "question_id": question_id, "manifest_sha256": manifest_hash,
            "status": "retrieval_complete" if success else "failed", "stage": stage,
            "expected_ids_sha256": canonical_hash(exact), "sources": sources,
            "artifact_fingerprints": {key: config.get(key) for key in FINGERPRINTS},
            "effective_config_sha256": getattr(adapter, "effective_config_sha256", None) if initialized else None}


def save_artifact(directory, artifact):
    """Create-only artifact; I/O failures propagate to stop the experiment."""
    path = Path(directory) / (canonical_hash(artifact) + ".json")
    with path.open("x") as handle:
        json.dump(artifact, handle, indent=2, sort_keys=True)
        handle.write("\n")
