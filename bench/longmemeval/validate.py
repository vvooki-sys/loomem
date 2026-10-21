#!/usr/bin/env python3
"""Strict, offline LongMemEval run validation; standard library only."""
import argparse
import hashlib
import json
import re
from pathlib import Path
import sys


class ValidationError(ValueError):
    """Artifacts cannot safely be used for scoring or resume."""


def sha256(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def canonical_hash(value):
    encoded = json.dumps(value, sort_keys=True, separators=(",", ":"),
                         ensure_ascii=False, allow_nan=False).encode()
    return hashlib.sha256(encoded).hexdigest()


def unique_object(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValidationError(f"Duplicate JSON key: {key}")
        result[key] = value
    return result


def decode(text, source):
    try:
        return json.loads(text, object_pairs_hook=unique_object,
                          parse_constant=lambda value: fail(f"Invalid JSON constant: {value}"))
    except (ValueError, TypeError) as exc:
        raise ValidationError(f"{source}: invalid JSON ({exc})") from exc


def fail(message):
    raise ValidationError(message)


def dataset_index(path):
    records = decode(Path(path).read_text(), "dataset")
    if not isinstance(records, list) or not records:
        fail("Dataset must be a nonempty JSON array")
    index = {}
    for record in records:
        if not isinstance(record, dict):
            fail("Dataset rows must be objects")
        qid, category = record.get("question_id"), record.get("question_type")
        if not isinstance(qid, str) or not qid or not isinstance(category, str) or not category:
            fail("Dataset requires nonempty question_id and question_type strings")
        if qid in index:
            fail(f"Duplicate dataset question_id: {qid}")
        index[qid] = category
    return index


def validate_config(settings):
    if not isinstance(settings, dict):
        fail("Run config must be a JSON object")
    hashes = {"engine_sha": (40, 64), "harness_sha": (40, 64),
              "reader_prompt_sha256": (64,), "judge_prompt_sha256": (64,),
              "execution_config_sha256": (64,)}
    for field, lengths in hashes.items():
        value = settings.get(field)
        if (not isinstance(value, str) or len(value) not in lengths
                or re.fullmatch(r"[0-9a-f]+", value) is None):
            fail(f"Run config {field} must be a full lowercase hexadecimal hash")
    for field in ("reader_model", "judge_model", "embedding_model"):
        value = settings.get(field)
        if not isinstance(value, str) or not value.strip():
            fail(f"Run config {field} must be a nonempty string")
    budget = settings.get("context_budget_tokens")
    if type(budget) is not int or budget <= 0:
        fail("Run config context_budget_tokens must be a positive integer")
    if type(settings.get("seed")) is not int:
        fail("Run config seed must be an integer")


def build_manifest(dataset, config):
    index = dataset_index(dataset)
    settings = decode(Path(config).read_text(), "run config")
    validate_config(settings)
    return {"schema_version": 2, "dataset_sha256": sha256(dataset),
            "config_sha256": canonical_hash(settings),
            "expected_ids_sha256": canonical_hash(sorted(index)),
            "expected_count": len(index)}


def check_manifest(manifest_path, dataset, config):
    manifest = decode(Path(manifest_path).read_text(), "manifest")
    expected = build_manifest(dataset, config)
    if manifest != expected:
        fail("Manifest mismatch: dataset, run config, expected IDs or schema changed")
    return canonical_hash(manifest)


def row_label(row, number):
    # Support native strict records and the official evaluator label envelope.
    if "correct" in row:
        label = row["correct"]
        if "autoeval_label" in row:
            fail(f"Line {number}: ambiguous label fields")
    else:
        envelope = row.get("autoeval_label")
        if not isinstance(envelope, dict):
            fail(f"Line {number}: missing judge label")
        label = envelope.get("label")
    if type(label) is not bool:
        fail(f"Line {number}: judge label must be a JSON boolean")
    return label


def read_results(path, index, manifest_hash):
    results = {}
    for number, line in enumerate(Path(path).read_text().splitlines(), 1):
        if not line.strip():
            fail(f"Line {number}: empty JSONL record")
        row = decode(line, f"Line {number}")
        if not isinstance(row, dict):
            fail(f"Line {number}: expected object")
        qid = row.get("question_id")
        if not isinstance(qid, str) or qid not in index:
            fail(f"Line {number}: unknown or invalid question_id")
        if qid in results:
            fail(f"Line {number}: duplicate question_id {qid}")
        if row.get("manifest_sha256") != manifest_hash:
            fail(f"Line {number}: manifest hash mismatch")
        if "question_type" in row and row["question_type"] != index[qid]:
            fail(f"Line {number}: category differs from dataset")
        status = row.get("status", "completed")
        if status == "failed":
            if "correct" in row or "autoeval_label" in row:
                fail(f"Line {number}: failed records cannot carry judge labels")
            if not isinstance(row.get("failure_stage"), str) or not row["failure_stage"].strip():
                fail(f"Line {number}: failed record requires failure_stage")
            results[qid] = None
        elif status == "completed":
            results[qid] = row_label(row, number)
        else:
            fail(f"Line {number}: unknown status")
    return results


def counts(ids, results):
    observed = [results[qid] for qid in ids if qid in results]
    completed = sum(value is not None for value in observed)
    correct = sum(value is True for value in observed)
    return {"expected": len(ids), "observed": len(observed),
            "completed": completed, "failed": len(observed) - completed,
            "missing": len(ids) - len(observed), "correct": correct,
            "accuracy": correct / len(ids) if ids else None,
            "completion_rate": completed / len(ids) if ids else None,
            "completed_qa_accuracy": correct / completed if completed else None}


def validate(dataset, config, manifest, results, allow_incomplete=False):
    manifest_hash = check_manifest(manifest, dataset, config)
    index = dataset_index(dataset)
    observations = read_results(results, index, manifest_hash)
    missing = sorted(set(index) - set(observations))
    if missing and not allow_incomplete:
        fail(f"Missing {len(missing)} expected question IDs; use --allow-incomplete only for resume inspection")
    return {"schema_version": 1, "accepted_complete_run": not missing,
            "manifest_sha256": manifest_hash, "results_sha256": sha256(results),
            "dataset_sha256": sha256(dataset), "summary": counts(index, observations),
            "categories": {category: counts([qid for qid in index if index[qid] == category], observations)
                           for category in sorted(set(index.values()))},
            "abstentions": counts([qid for qid in index if qid.endswith("_abs")], observations),
            "missing_ids": missing,
            "resume_skip_ids": sorted(qid for qid, value in observations.items() if value is not None),
            "retry_failed_ids": sorted(qid for qid, value in observations.items() if value is None)}


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=["manifest", "validate"])
    parser.add_argument("--dataset", required=True, type=Path)
    parser.add_argument("--config", required=True, type=Path)
    parser.add_argument("--manifest", required=True, type=Path)
    parser.add_argument("--results", type=Path)
    parser.add_argument("--allow-incomplete", action="store_true")
    args = parser.parse_args(argv)
    try:
        if args.command == "manifest":
            manifest = build_manifest(args.dataset, args.config)
            # A run's identity is immutable: never overwrite an existing manifest.
            with args.manifest.open("x") as handle:
                handle.write(json.dumps(manifest, indent=2) + "\n")
            result = {"manifest_sha256": canonical_hash(manifest)}
        else:
            if args.results is None:
                parser.error("validate requires --results")
            result = validate(args.dataset, args.config, args.manifest,
                              args.results, args.allow_incomplete)
    except (OSError, ValueError) as exc:
        print(f"Validation failed: {exc}", file=sys.stderr)
        return 2
    print(json.dumps(result, indent=2, allow_nan=False))
    return 0


if __name__ == "__main__":
    sys.exit(main())
