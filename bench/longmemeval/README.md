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

## Offline source/fact representation experiment

`representations.py` provides `build_records(question, mode, facts=None)` for
`raw`, `facts`, and `dual`. The same history supplies stable question-scoped
record IDs and explicit session/zero-based turn references, speaker role and
session date (not event date). Facts must be supplied as a separate JSON list of
`{"content": "…", "sources": [{"session_id": "…", "turn_index": 0}]}`; optional
`role` and `date` are copied only when explicitly present. Empty/missing `sources`
means unknown legacy provenance; it never reconstructs a source from a fact.
References to nonexistent turns and ambiguous session/date alignment are rejected.
This is an offline benchmark representation, not a change to product retention,
extraction, deletion policy or storage. Use synthetic or authorized data only.

`serialize_records(records)` allowlists evidence fields: neither `answer` nor
`has_answer` nor other gold metadata is copied into generation input. External
fact text must itself come from a label-blind extraction process; code cannot
prove how a supplied fixture was authored. Source text is untrusted data and must
be framed as such by the reader prompt.

`pack_records(records, budget_tokens, count_tokens)` requires the caller's actual
model-tokenizer callback and counts the entire serialized evidence array,
including IDs and metadata. It preserves full content, skips oversized records,
and suppresses only records with identical content, provenance, kind, role and
date. It returns the exact selected records/context, callback count and dropped
IDs. Even the empty envelope must fit. This is an evidence-context budget;
question, system prompt and completion need separate accounting. Test callbacks
using character counts validate the packing contract only, not real token costs.
No built-in approximation is presented as a model tokenizer.

Evaluation is separate from generation. Given one question JSON object and the
JSON array of records actually selected for context:

```sh
python3 bench/longmemeval/evidence.py \
  --question /absolute/path/question.json \
  --records /absolute/path/selected-records.json
```

Diagnostics report session provenance recall, turn provenance recall and whether
**all** supporting IDs are present. `full_source_turn` counts only source records
whose entire text matches the original labeled turn. A linked fact can omit a
negation or exception: provenance alone never certifies semantic support.
Missing labels, empty support sets and incomplete/unknown record provenance yield
`null` rather than fabricated zero/one scores. `semantic_support` remains
`not_measured`; these diagnostics are not QA accuracy. Synthetic PL/EN tests cover
omitted exceptions, chronology, negation in later sentences, duplicate content
across different origins, partial support and strict budget behavior.

## Local retrieval runner (experimental)

`runner.py` runs **ingest → readiness → retrieval only**, using Python's standard
library. It never invokes a reader/judge, computes QA accuracy, or enforces a
reader token budget. Its JSONL is an ID-only diagnostic sidecar, **not** scored
results accepted by `validate.py`. Successful rows use `retrieval_complete`;
failures carry their actual stage and produce exit 2. No wipe/delete/purge API
exists in this runner. Use public benchmark or synthetic data only.

Add a `runner` object to the frozen run config before creating its manifest:

- `endpoint`: literal loopback HTTP URL with explicit port (no DNS, path,
  credentials, proxies or redirects).
- `instance_id`, `run_id`: explicit nonempty experiment identities.
- `data_dir`: absolute experiment-only output directory.
- `mode`: `raw`, `facts`, or `dual`; `top_k`: positive integer.
- For `facts`/`dual`, add `facts_path` and `facts_sha256`. The UTF-8 JSON file maps
  question IDs to externally supplied fact lists accepted by `representations.py`.
  Facts are never inferred from gold answers. The file hash is checked before I/O.

```sh
python3 bench/longmemeval/runner.py \
  --dataset /absolute/path/dataset.json --config /absolute/path/run-config.json \
  --manifest /absolute/path/run-manifest.json --trace /absolute/path/trace.jsonl \
  --dry-run
```

Dry-run validates the manifest, frozen artifacts and every source projection. It
makes no network requests, launches no processes and creates no output. Remove
`--dry-run` to execute. `--resume` rejects mismatched manifests, duplicate/unknown
IDs, malformed stages and out-of-scope IDs. It skips **both successes and explicit
failures**, preventing accidental replay after partial writes. Retry failures
with a new output sidecar (and, for an external adapter, a new isolated run).
An interrupted run can leave missing IDs; resume processes those IDs. Never
append a replacement for an existing question.

### Owned Loomem process

For an actual engine run, add `local_engine` inside `runner` with exactly:
`binary`, `binary_sha256`, `template`, `template_sha256`, `model_dir`,
`model_onnx_sha256`, `tokenizer_sha256`. Paths must be absolute. Hash the actual
server executable, config template, `model.onnx`, and `tokenizer.json`. Use the
local multilingual-e5-small model (384 dimensions). The endpoint must be
`http://127.0.0.1:PORT`; an occupied port is rejected.

`runner_local.py` creates an exclusive random invocation directory and a **fresh
DB per question**, launches the supplied executable, and terminates only that
owned process after each question. It retains databases/configs for inspection;
it never removes or reuses previous data. The subprocess receives fresh HOME,
random local authentication and no inherited API credentials/proxies. Generated
configuration forces local embeddings, empty completion credentials, isolated
WAL/event paths and empty entity/synonym files; it disables scheduler, extraction,
dream, contradiction, content typing, reranking and query expansion. This is a
**raw-store representation experiment**, not a test of full online extraction or
consolidation. Network isolation is achieved by the controlled engine config and
credential removal, not an operating-system network sandbox; the supplied binary
and template must be trusted.

Readiness requires exact equality of expected successful stores, persisted chunk
count, Tantivy count and embedding count. This barrier applies only to the
controlled raw-store configuration with other writers disabled. It must not be
reused as proof of general extraction/consolidation readiness. Startup and
indexing deadlines are 45 seconds; failures never become empty successful reads.

### External local adapter contract

Without `local_engine`, the runner requires an explicitly provided benchmark
adapter; this is **not** the ordinary Loomem REST API. It checks `GET /identity`
returns exactly `{schema_version:1, dedicated:true, network_policy:"offline",
paid_calls_enabled:false, instance_id, data_dir, run_id}`, matching frozen config.
This is an adapter attestation, not independent process verification.

`POST /ingest` receives `{scope, records}` and must return
`{status:"complete", accepted_ids:[...]}` with every supplied record ID exactly
once. `POST /ready` receives `{scope, expected_ids}` and must return
`{ready:true, pending_jobs:0, indexes_ready:true, indexed_ids:[...]}` covering all
records. `POST /search` receives only `{scope, query, top_k, trace:true}` and returns
`{retrieved_ids:[...], trace_metadata?:{evidence:...}}`. Scope is a digest of the
manifest and question ID. No gold answer, question category, abstention suffix or
`has_answer` annotation is forwarded. An external adapter must isolate that scope
and enforce its claimed offline policy.

Trace evidence uses schema v1, coverage `post_rank_to_response`, and ordered
stages `ranked_candidates`, `after_soft_delete`, `after_filters_and_dedup`,
`response`, each with `chunk_ids`. The final stage must equal returned IDs in
order; every ID must belong to the question's supplied records. The owned adapter
maps engine chunk IDs to representation record IDs. Missing engine evidence is
explicit `coverage:"unavailable"`; it never implies stage completeness. Sidecars
contain no source text, query, exception body, answer or model output. Original
record IDs demonstrate provenance only: a later reader must consume actual
sanitized engine evidence, not reconstruct potentially different pre-sanitization
text from those IDs.
