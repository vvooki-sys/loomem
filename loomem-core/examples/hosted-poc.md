# Hosted synthetic shared-engine experiment

This private-IPC example is a synthetic proof of concept, not a server or a
security boundary. One process holds multiple explicitly addressed Hosted
scopes and uses Loomem's local ONNX embedder, RocksDB, Tantivy, vector search
and hybrid scoring. It does not implement Hosted authorization, accounts,
publication, journal, recovery, OS process isolation, encryption, sanitization,
or the complete ingest pipeline. Never expose it to clients or real data.

Build with `cargo build -p loomem-core --example hosted_poc_worker`. Start the
binary with `SHARED_ROOT CONFIG MODEL_DIR`. `SHARED_ROOT` must be an existing
private directory containing one existing private subdirectory per scope;
`CONFIG` an unmodified compatible Loomem TOML; and `MODEL_DIR` a local
384-dimension model directory containing `model.onnx` and `tokenizer.json`.
The supervisor must use a clean environment without credentials (including
`LOOMEM_AT_REST_MASTER_KEY`) and enforce all filesystem and process boundaries.

Every operation except shutdown carries an explicit context:

```json
{"actor":"actor-a","scope":"scope-a","epoch":1}
```

The scope selects a separate storage, index and asynchronous-job namespace in
the same engine process. Responses and rejected operations echo that context
and the engine PID. Actor/scope/epoch are binding data supplied by Hosted, not
an authorization decision made by Loomem.

Requests:

* `{"cmd":"open_scope","context":{...}}` opens `SHARED_ROOT/scope` without
  starting another engine. An internal Hosted reader request may supply a
  disposable generation-copy `root`; retargeting is rejected while that scope
  has active jobs.
* `{"cmd":"prepare","context":{...},"op_id":"op1","generation":"g1","content":"synthetic text"}`
  returns the model's 384-value `embedding` with the same binding.
* `{"cmd":"build_async","context":{...},"generation":"g1","records":[{"op_id":"op1","id":"m1","content":"synthetic text","embedding":[...]}]}`
  starts one scoped job on an operation thread and returns `pending` and `job`.
  It does not start a process or worker per scope. The caller supplies the
  complete journal-derived record set. Optional `failpoint` `after_rocksdb`
  fails the job, `after_rocksdb_abort` terminates the shared process, and
  `after_rocksdb_wait` emits `LOOMEM_POC_BARRIER_AFTER_ROCKSDB` on stderr then
  waits for SIGKILL. No ready marker is written on those paths.
* `{"cmd":"await","context":{...},"job":"g1"}` joins a job only from the same
  context's queue and returns its validated generation result.
* `{"cmd":"search","context":{...},"generation":"g1","query":"synthetic","embedding":[...]}`
  returns separate `vector`, `bm25`, and original Loomem `hybrid` results.
  Empty vector results are not silently treated as successful vector retrieval.
* `{"cmd":"read","context":{...},"generation":"g1","id":"m1"}` returns `chunk`
  or null from that scope only.
* `{"cmd":"status","context":{...}}` returns the number of open contexts.
* `{"cmd":"shutdown"}` exits the shared process cleanly.

Search and read open Loomem's original mutable storage adapters. Hosted uses
them only on disposable physical copies of a previously accepted generation,
never on the authoritative immutable generation. A `READY` marker is only a
completeness check, not authorization or fencing. Failures return
`{"ok":false,"error":"operation failed"}` with the context when it was valid.
A terminated or timed-out process has an uncertain outcome: Hosted must
reconcile its canonical journal and may not blindly retry a generation name.
All context, generation and record identifiers are bounded ASCII letters,
digits, underscore or hyphen.
