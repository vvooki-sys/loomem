# Offline LongMemEval result validation

`validate.py` checks artifact integrity before scoring or resuming a run. Python
3.10+ standard library only; it sends no requests, starts no engine and modifies
no datasets/results. This is a validator, not an evaluator or benchmark runner.
It does not reconstruct missing historical provenance.

## Freeze a run

Supply the existing LongMemEval dataset JSON array and a separate run-config JSON
object. The full dataset defines expected IDs: use a separately prepared dataset
subset when intentionally evaluating a subset. No benchmark data belongs in Git.

The run config must contain the following named fields (manifest schema v2):

| Field | Required value |
|---|---|
| `engine_sha`, `harness_sha` | Full lowercase hexadecimal Git object IDs (40 or 64 characters) |
| `reader_prompt_sha256`, `judge_prompt_sha256` | SHA-256 of the exact frozen prompt artifacts, 64 lowercase hexadecimal characters |
| `execution_config_sha256` | SHA-256 of the frozen execution-settings artifact, 64 lowercase hexadecimal characters |
| `reader_model`, `judge_model`, `embedding_model` | Nonempty model identifiers; use immutable snapshots where available |
| `context_budget_tokens` | Positive integer, not a JSON boolean |
| `seed` | Integer, not a JSON boolean |

Compute actual hashes from the frozen files/commits; do not substitute example
hashes. Execution settings should record effective engine configuration,
generation parameters, clock, worker/readiness policy, cache state, dependency
versions and hardware. Keep the hashed artifacts alongside the run. Additional
config fields are allowed and also bound by the manifest. Never put credentials
or private endpoints in these artifacts. The validator enforces presence and
format, but cannot prove a referenced artifact exists or that a runner actually
used it. This minimum metadata gate alone does not certify a publishable result.

```sh
python3 bench/longmemeval/validate.py manifest \
  --dataset /absolute/path/longmemeval_s_cleaned.json \
  --config /absolute/path/run-config.json \
  --manifest /absolute/path/run-manifest.json
```

The command creates a new manifest exclusively (existing paths are rejected) and
prints its canonical `manifest_sha256`. Dataset bytes, sorted expected IDs and
canonical config contents are bound to the manifest. Changes to config values or
dataset bytes require a new run. Reordering config keys preserves identity.

## Result contract

Each JSONL record must carry a unique dataset question ID and the printed manifest
hash. A completed record contains a **JSON boolean** `correct`, or the official
`autoeval_label` object containing boolean `label`, but never both. For example:

```json
{"question_id":"synthetic_a","manifest_sha256":"HASH_FROM_MANIFEST_COMMAND","status":"completed","correct":true}
```

For infrastructure failures record `status: "failed"` and a nonempty
`failure_stage` (e.g. `ingest`, `readiness`, `retrieval`, `reader`, `judge`), with
no judge label. Failures stay in the denominator and do not masquerade as reader
abstention. If supplied, `question_type` must match the dataset. Extra fields such
as hypothesis or usage are permitted but not interpreted or printed.

```sh
python3 bench/longmemeval/validate.py validate \
  --dataset /absolute/path/longmemeval_s_cleaned.json \
  --config /absolute/path/run-config.json \
  --manifest /absolute/path/run-manifest.json \
  --results /absolute/path/results.jsonl
```

Exit 0 means the requested validation passed; exit 2 means invalid artifacts.
Malformed/blank lines, duplicate keys/IDs, unknown IDs, nonboolean labels,
conflicting labels, category mismatches and manifest mismatches always fail.
Missing IDs fail unless `--allow-incomplete` is explicitly supplied for resume
inspection. That mode returns `accepted_complete_run: false` until all expected
IDs are represented. Empty files are valid only for initial resume inspection.

The summary always uses **all expected questions** as denominator, including
missing/failed rows; it reports completion, failure and missing counts separately. `completed_qa_accuracy` is correct answers divided only by
completed, judged answers; it is a diagnostic metric and must not replace the
full-denominator `accuracy`. Failed and missing rows contribute zero correct
answers to that main score. Any metric with zero denominator (e.g. no abstention
questions or no completed answers) is JSON `null`, never a misleading zero.
`accepted_complete_run` means full record coverage, not 100% completion: a full
run containing explicit failures has lower completion and accuracy. Categories
come from the dataset; `_abs` is a separate overlapping slice. Output includes
file SHA-256 hashes, safe-to-skip completed IDs, failed IDs to retry and missing
IDs. A failed retry must replace its prior row in a newly materialized result
file, not append a duplicate; this tool never rewrites results.

Historical files without manifest bindings or with numeric 0/1 labels are
intentionally rejected. Do not add a current manifest hash to old rows and claim
reproducibility. Explicit migration needs independently established provenance.
Hashes detect accidental mismatches, not fabricated or malicious records.

Tests use synthetic data only:

```sh
python3 -m unittest discover -s bench/longmemeval -p 'test_*.py' -v
```
