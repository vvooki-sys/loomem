# Configuration Reference

All settings live in `config.toml`. There are no hardcoded defaults — every value must be explicitly configured.

Override config file path with `LOOMEM_CONFIG` environment variable.

Authentication is configured via `[server].auth_token_env` (see [\[server\]](#server)) — there is no separate `[auth]` section.

---

## [storage]

```toml
[storage]
data_dir = "./data"
vector_enabled = true
```

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `data_dir` | string | "./data" | Root directory for RocksDB, Tantivy, embeddings |
| `vector_enabled` | bool | true | Enable vector (embedding) search |

### [storage.rocksdb]

```toml
[storage.rocksdb]
max_open_files = 1000
compression = "lz4"
write_buffer_size = 67108864
max_write_buffer_number = 3
```

| Key | Type | Description |
|-----|------|-------------|
| `max_open_files` | int | File descriptor pool size |
| `compression` | string | `"lz4"` or `"snappy"` — LZ4 recommended |
| `write_buffer_size` | int | Bytes per write buffer (64 MB) |
| `max_write_buffer_number` | int | Number of write buffers |

### [storage.tantivy]

```toml
[storage.tantivy]
enabled = true
heap_size_mb = 128
```

| Key | Type | Description |
|-----|------|-------------|
| `enabled` | bool | Enable BM25 full-text search |
| `heap_size_mb` | int | Index writer memory pool |

### [storage.intent_log]

Write-ahead log for cross-store consistency.

```toml
[storage.intent_log]
enabled = true
dir = "wal"
max_size_mb = 10
sync_on_write = false
```

| Key | Type | Description |
|-----|------|-------------|
| `enabled` | bool | Enable WAL |
| `dir` | string | WAL directory (relative to data_dir) |
| `max_size_mb` | int | Rotate when log reaches this size |
| `sync_on_write` | bool | fsync every write (safer but slower) |

---

## [search]

```toml
[search]
top_k = 10
max_request_top_k = 1000      # HTTP top_k must be 1..this limit; MCP caps are separate
surprise_boost = 1.5
synonyms_file = "synonyms.toml"
entities_file = "entities.toml"
rerank_enabled = false
rerank_candidates = 20
rerank_model_dir = "models/reranker"
multi_query_enabled = false
stem_polish = true
evidence_fields = false
```

| Key | Type | Description |
|-----|------|-------------|
| `top_k` | int | Default number of search results |
| `max_request_top_k` | int | HTTP result limit ceiling (default 1000); zero or requests above it return 400 before retrieval. MCP caps remain separate. |
| `surprise_boost` | f64 | Novelty multiplier on ingest (Titans-inspired) |
| `synonyms_file` | string | Path to synonym expansion map |
| `entities_file` | string | Path to entity dictionary |
| `rerank_enabled` | bool | Enable cross-encoder reranking (~97ms/pair) |
| `rerank_candidates` | int | Send top N to reranker |
| `rerank_model_dir` | string | ONNX model directory |
| `multi_query_enabled` | bool | Decompose complex queries into sub-queries |
| `stem_polish` | bool | Enable Polish language stemming |
| `evidence_fields` | bool | **Experimental, opt-in** (default `false`). When `true`, every `/v1/search` hit and `GET /api/memories/{id}` item carries an additive `evidence` object (`author`, `conversation_date`, `event_date`, `event_date_provenance`, `source_id`, `byte_start`, `byte_end`), and each `memory_search` line carries a bracketed evidence tag before the score. When `false`, responses are byte-identical to pre-flag output. See [API Reference](api-reference.md#post-v1search). |

### Quality levers

```toml
[search]
rerank_date_prefix = false
rerank_llm_max_chars = 200
rerank_onnx_max_tokens = 0
rerank_sync = false
rerank_sync_timeout_ms = 2000
event_date_proximity_boost = 0.0
content_dedup = "prefix"
```

These `[search]` keys, the `memory_search` keys in [`[mcp]`](#mcp) and `temporal_grounding` in [`[knowledge_extraction]`](#knowledge_extraction) are quality levers. All of them are off or neutral by default: with the shipped values, output is byte-identical to the output before the key existed. A default changes only after a measured A/B run. A value the validation rejects stops the config from loading.

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `rerank_date_prefix` | bool | `false` | Prefix each document sent to the reranker with the same date tag `memory_search` lines show (`[event:YYYY-MM-DD]`, `[observed:…]` or `[recorded:…]`, else the extracted event date, else the hit's timestamp). Costs one chunk read per reranked candidate. With the LLM reranker the prefix counts toward `rerank_llm_max_chars`. `false`: the reranker sees the raw content |
| `rerank_llm_max_chars` | int | `200` | Characters (not bytes) of each document sent to the LLM fallback reranker, which runs when no ONNX cross-encoder is loaded. The ONNX cross-encoder ignores it. Must be positive. `200` is the value that was hardcoded before |
| `rerank_onnx_max_tokens` | int | `0` | Token cap for each (query, document) pair the ONNX cross-encoder scores, special tokens included. Tokens come off the longer sequence first (normally the document); once both are the same length, both are cut. `0`: no truncation, and a pair over the model's limit fails the async rerank as before. Any other value must be **at least 16**; smaller values are rejected by validation. Ignored, with a log warning, when the model's `tokenizer.json` already sets its own truncation |
| `rerank_sync` | bool | `false` | Run the reranker on the request path and return its order in the same response. `false`: the rerank runs in the background, its order is cached for the next identical query, and the current response keeps the fused order. Only acts when `rerank_enabled = true` and the cost budget allows the reranker. With `true`, requests that cannot be cached are reranked too. On a model error or timeout the response keeps the fused order. A hit the reranker pushes out of the top-k is traced with the rejection reason `reranked` (see [API Reference](api-reference.md#post-v1search)). Read the warning below before enabling it |
| `rerank_sync_timeout_ms` | int | `2000` | Time budget in milliseconds for the synchronous rerank's model call only. Building the documents, waiting on the cache lock and the store reads after the wait come on top. Must be positive. Inert unless `rerank_sync = true` |
| `event_date_proximity_boost` | f64 | `0.0` | Weight of a score multiplier for hits whose event date falls inside or near the request's date range (`date_from`/`date_to`, or a date parsed from the query text). Inside the range, edges included, the score is multiplied by `1 + weight`. Outside it the boost falls off linearly per whole UTC day and reaches `1.0` at `n + 1` days from the range, where `n` is the range length in days, capped at 365. Hits without an event date, and requests without a range, are unchanged. Must be finite and non-negative; `0.0` disables it. The date filter still runs afterwards, so the fall-off outside the range only reorders hits the filter keeps (legacy records, which it does not drop by date). Temporal queries are re-sorted by timestamp after scoring, so for them the boost changes scores but not order |
| `content_dedup` | string | `"prefix"` | How near-duplicate results are collapsed. `"prefix"` (the previous behaviour): compare the first 150 characters of each result, lowercased, and drop the lower-ranked one when the two share a leading run longer than 80% of the shorter prefix, with both prefixes at least 30 bytes long. The run is counted in characters but the 80% is taken of the length in bytes, so text with many multi-byte characters (such as Polish letters) collapses less often, and two identical prefixes can both be kept. Fragments of different sources, and a fragment against its own source, never collapse. `"exact"`: collapse only byte-identical plain chunks of at least 30 bytes, and never fragments, so a negation or update after a shared prefix survives. Any other value fails config loading. In a trace, results dropped in either mode keep the rejection reason `content_prefix_dedup` |

> **Warning: `rerank_sync`.** Do not enable it until a cap on in-flight synchronous reranks exists (a planned follow-up).
> - On timeout, ONNX scoring is not cancelled: it keeps running in the blocking thread pool and its result is discarded. With `rerank_candidates = 20` at ~97 ms per pair (~1.94 s per search), concurrent searches can pile such orphaned jobs up.
> - `rerank_sync_timeout_ms` covers only the model call, not the work around it (see the table).
> - With the LLM reranker, sync mode also reranks requests that cannot be cached, and that LLM spend is not counted by the cost tracker.

### [search.cache]

```toml
[search.cache]
enabled = true
max_entries = 500
ttl_secs = 300
```

Semantic query cache. Identical queries within TTL return cached results.

### [search.graph]

```toml
[search.graph]
enabled = true
max_hops = 1
boost_factor = 0.3
max_graph_additions = 3
```

| Key | Type | Description |
|-----|------|-------------|
| `enabled` | bool | Enable graph-enhanced search |
| `max_hops` | int | 1 = direct neighbors, 2 = 2-hop |
| `boost_factor` | f64 | Score multiplier for graph-discovered results (0.0 – 1.0) |
| `max_graph_additions` | int | Max graph-only results added to search |

### [search.hybrid_weights]

```toml
[search.hybrid_weights]
vector = 0.6
bm25 = 0.4
```

Controls the fusion ratio. Must sum to 1.0.

### [search.decay]

```toml
[search.decay]
l0_lambda = 0.05
l1_lambda = 0.03
```

Exponential decay rate per tier. Higher = faster decay. Half-life ≈ ln(2) / lambda days.

| Tier | Lambda | Approximate half-life |
|------|--------|-----------------------|
| L0 | 0.05 | ~14 days |
| L1 | 0.03 | ~23 days |

### [search.complexity]

```toml
[search.complexity]
enabled = false
simple_top_k = 3
medium_top_k = 10
complex_top_k = 20
```

Complexity-aware routing. When enabled, adjusts `top_k` based on query complexity classification. Currently disabled — all queries use full pipeline.

---

## [worker]

### [worker.consolidation]

```toml
[worker.consolidation]
interval_secs = 300
batch_size = 200
concurrency = 2
timeout_secs = 300
min_chunks_to_consolidate = 3
min_age_secs = 60
prompt_version = 1
consolidation_style = "structured"
similarity_threshold = 0.20
```

| Key | Type | Description |
|-----|------|-------------|
| `interval_secs` | int | How often to run (5 min) |
| `batch_size` | int | Max chunks per stream per run |
| `concurrency` | int | Parallel streams |
| `min_chunks_to_consolidate` | int | Skip streams with fewer chunks |
| `min_age_secs` | int | Only consolidate chunks older than this |
| `consolidation_style` | string | `"structured"` = typed facts with date resolution, `"observation"` = granular facts, `"summary"` = paragraph |
| `similarity_threshold` | f64 | Cosine similarity for topic grouping before compress (0.0 = one per chunk, 1.0 = all in one) |

### [worker.decay_worker]

```toml
[worker.decay_worker]
interval_secs = 3600
factor = 0.995
l0_factor = 0.990
l1_factor = 0.995
dormant_threshold = 0.01
access_boost = true
adaptive_enabled = true
adaptive_dampening = 0.5
adaptive_cap = 200
```

| Key | Type | Description |
|-----|------|-------------|
| `interval_secs` | int | Run every hour |
| `l0/l1_factor` | f64 | Decay multiplier per hour per tier |
| `dormant_threshold` | f64 | Score below this = marked dormant |
| `access_boost` | bool | Reset score on search hit |
| `adaptive_enabled` | bool | ACT-R: frequently accessed decay slower |
| `adaptive_dampening` | f64 | How much access_count influences decay |
| `adaptive_cap` | int | access_count ceiling for adaptive effect |

### [worker.compaction]

```toml
[worker.compaction]
interval_secs = 3600
timeout_secs = 300
```

RocksDB background compaction trigger.

### [worker.clustering]

```toml
[worker.clustering]
interval_secs = 21600
max_iterations = 1000
timeout_secs = 600
```

Clustering worker runs k-means on L1 embeddings to group related memories.

---

## [scheduler]

```toml
[scheduler]
enabled = true
```

Master switch for all background workers.

---

## [llm]

```toml
[llm]
provider = "openai"                  # completions: a [llm.providers.<name>] entry, or the built-in "openai"
api_key_env = "OPENAI_API_KEY"       # legacy key field: OpenAI embeddings + fallback for the built-in provider
embedding_provider = "local"         # "local" (on-device ONNX) or "openai"
embedding_model = "text-embedding-3-small"   # used when embedding_provider = "openai"
# embedding_model_path = "/abs/path"  # local model dir; default ~/.loomem/models/multilingual-e5-small
embedding_dim = 384                  # MUST match the active embedding model
compression_model = "gpt-6-luna"     # chat model for every LLM-backed step
timeout_secs = 10                    # shared HTTP client (OpenAI embeddings); chat calls have their own per-request timeout
fallback_to_regex = true

[llm.providers.openai]
base_url = "https://api.openai.com/v1"
api_key_env = "OPENAI_API_KEY"
max_completion_tokens = true
reasoning_effort = "low"
omit_temperature = true
completion_tokens_floor = 2000  # reasoning tokens share the small per-call caps (smoke test, 2026-11-22)
```

| Key | Type | Description |
|-----|------|-------------|
| `provider` | string | Completions provider for every LLM-backed step: the name of a [`[llm.providers.<name>]`](#llmprovidersname) entry, or the built-in `"openai"` (`https://api.openai.com/v1`) when no entry of that name exists |
| `embedding_provider` | string | `"local"` (on-device ONNX, no API key) or `"openai"`. Missing in older configs → treated as `"openai"` |
| `api_key_env` | string | Legacy key field: read by `embedding_provider = "openai"` and by the built-in `"openai"` provider. A `[llm.providers.openai]` entry never reads it; only an inline `api_key` is that entry's fallback |
| `embedding_model` | string | Embedding model name when `embedding_provider = "openai"` |
| `embedding_model_path` | string? | Directory with `model.onnx` + `tokenizer.json` for local embeddings. Unset → `~/.loomem/models/multilingual-e5-small` |
| `embedding_dim` | int | Embedding vector dimensions — **must match the active model** |
| `compression_model` | string | Chat model for every LLM-backed step (consolidation, extraction, dream, …); the per-feature `model` keys inherit it when unset |
| `timeout_secs` | int | Timeout of the shared HTTP client (OpenAI embeddings); every chat call carries its own per-request timeout |
| `fallback_to_regex` | bool | Consolidation only: regex compression when the completions API call fails (does not cover a missing key) |

### Local embeddings (default)

Fresh installs use **`embedding_provider = "local"`**: embeddings are computed
on-device with a quantization-free ONNX model (default
`multilingual-e5-small`, 384-dim, good multilingual/Polish recall) via the pure-Rust
`tract` runtime. No API key is required and no text leaves the machine. The model
ships in the default build; obtain the model files with:

```sh
./scripts/fetch-embedding-model.sh         # → ~/.loomem/models/multilingual-e5-small
```

The completions LLM (consolidation, fact extraction, dream) is independent. With
the default config (those subsystems enabled) and no key for the active provider
(`OPENAI_API_KEY` with the shipped config), the server
refuses to start unless `LOOMEM_ALLOW_KEYLESS=1` is set, and then `memory_ingest`
may store the source text but returns a missing-key error and extracts no facts;
LLM-based entity extraction is off and
dream/consolidation fail, while `memory_store` and semantic search work fully
locally. Disabling those subsystems in `config.toml` lifts the startup check, and
with `knowledge_extraction.enabled = false` `memory_ingest` stores the raw
transcript instead. To use a local LLM for
completions too, point `provider` at an OpenAI-compatible endpoint declared in
[`[llm.providers.<name>]`](#llmprovidersname).

### Embedding dimension & re-embedding

`embedding_dim` **must** match the active embedding model
(`multilingual-e5-small` = 384, OpenAI `text-embedding-3-small` = 1536). The
dimension a database was built with is recorded in its metadata; on a mismatch
the server **refuses to start** rather than silently mixing vector sizes. To
switch providers/models on an existing database, re-embed it:

```sh
loomem-server --reembed   # recompute all vectors with the configured provider (run with the server stopped)
```

To use the API instead of local embeddings, set `embedding_provider = "openai"`,
`embedding_dim = 1536`, export `OPENAI_API_KEY`, and re-embed.

### [llm.providers.<name>]

Named OpenAI-compatible completion providers. `provider` picks one by name;
embeddings are not affected (they follow `embedding_provider`). The entry named
`openai` refines the built-in provider: its `api_key_env` wins, and only the
inline top-level `llm.api_key` is a fallback — the legacy `llm.api_key_env` is
never read for the entry, so an entry with its own, unset env var fails closed
instead of sending the OpenAI key elsewhere. A config without
this entry (every installation created before the switch to `gpt-6-luna`) keeps
the built-in provider and sends exactly the requests it sent before.

```toml
[llm.providers.openai]
base_url = "https://api.openai.com/v1"
api_key_env = "OPENAI_API_KEY"
max_completion_tokens = true
reasoning_effort = "low"
omit_temperature = true
completion_tokens_floor = 2000  # reasoning tokens share the small per-call caps (smoke test, 2026-11-22)

# [llm.providers.ollama]
# base_url = "http://localhost:11434/v1"   # no api_key_env: requests carry no key
```

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `base_url` | string | required | API root, e.g. `https://api.openai.com/v1` or `http://localhost:11434/v1`. Absolute `http(s)` URL; no query, fragment or credentials |
| `api_key_env` | string? | — | Environment variable holding the key. Omitted → no key: requests carry no `Authorization` header (Ollama, vLLM, a gateway) |
| `omit_temperature` | bool | false | Drop `temperature` from every chat request |
| `max_completion_tokens` | bool | false | Send the output cap as `max_completion_tokens` instead of the deprecated `max_tokens` |
| `completion_tokens_floor` | int | 0 | Raise any per-call output cap below this value to it (`0` = unchanged). The shipped `openai` entry sets `2000`: at `reasoning_effort = "low"` the NER batch (cap 1000) and the LLM rerank (cap 100) returned `finish_reason: "length"` with empty content, and with the floor at 2000 both completed (smoke test, 2026-11-22). The cap is a limit, not a spend; knowledge extraction has its own cap above the floor, so the floor never touches it |
| `reasoning_effort` | string? | — | One of `none`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max`; added to every chat request when set. The list is the union of values the Chat Completions reference documents, and a model may reject part of it with a 400 — `gpt-6-luna` accepts `none`, `low`, `medium`, `high`, `xhigh` and rejects `minimal` (API response, 2026-11-22). The config check rejects only values outside the list; the model's own answer is the final word |

An entry with none of the four request options set sends exactly the request the built-in provider sends.

For `gpt-6-luna` the shipped entry's `omit_temperature` and `max_completion_tokens` are required, not optional: the API rejects a `temperature` other than 1 and `max_tokens`, each with a 400 (smoke test, 2026-11-22).

The startup check and the `/health` field `llm_key_present` follow the active
provider: with the shipped config the server refuses to start without
`OPENAI_API_KEY` while `entity_extraction`, `knowledge_extraction` or `dream` are
enabled (unless `LOOMEM_ALLOW_KEYLESS=1`), and the error names the active
provider's env var. A provider without `api_key_env` (local Ollama) counts as
present and starts without `LOOMEM_ALLOW_KEYLESS`.

**Existing installations.** The installer never overwrites `config.toml`, so
nothing changes until you edit it. To switch to the new default: set
`compression_model = "gpt-6-luna"`, remove any per-feature `model = "gpt-4.1-mini"`
lines, and add the `[llm.providers.openai]` table above (once it exists, its
`api_key_env` takes precedence over an inline `llm.api_key`, which stays the
fallback). To stay on the old model:
nothing to do before 2027-01-14, when OpenAI retires `gpt-4.1-mini`.

---

## [server]

```toml
[server]
host = "127.0.0.1"
port = 3030
auth_token_env = "LOOMEM_AUTH_TOKEN"
```

| Key | Type | Description |
|-----|------|-------------|
| `host` | string | Bind address (`0.0.0.0` for production) |
| `port` | int | HTTP port (overridden by `PORT` env var) |
| `auth_token_env` | string | Name of the env var holding the API Bearer key (default `LOOMEM_AUTH_TOKEN`). If that env var is unset or empty, auth is disabled and the server runs in local passthrough mode |

---

## [mcp]

```toml
[mcp]
search_default_top_k = 5
search_max_top_k = 20
aggregation_default_top_k = 30
aggregation_max_top_k = 30
search_include_predecessor = false
predecessor_max_chars = 400
search_group_by = "none"
```

`top_k` limits and text rendering for the MCP `memory_search` tool. The last three keys are quality levers (see [Quality levers](#quality-levers)). None of them applies to aggregation (enumeration) queries, whose output stays a flat numbered list.

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `search_default_top_k` | int | `5` | `top_k` when a normal query omits it |
| `search_max_top_k` | int | `20` | Cap on `top_k` for normal queries |
| `aggregation_default_top_k` | int | `30` | `top_k` when an aggregation query omits it |
| `aggregation_max_top_k` | int | `30` | Cap on `top_k` for aggregation queries |
| `search_include_predecessor` | bool | `false` | Under a hit marked `[UPDATED]`, add one `↳ previously [<date>]: <text>` line with the version that hit directly superseded (`supersedes_id`, one hop). The predecessor line is **not** subject to `time_filter` or `valid_at`: it is shown even when its own date lies outside the requested range. It is left out when the predecessor is deleted, in another stream, missing, or already one of the hits. Its text counts toward the call's `max_context_tokens` |
| `predecessor_max_chars` | int | `400` | Character cap for the predecessor text; longer text is cut and ends with `…`. Must be positive. Inert unless `search_include_predecessor = true` |
| `search_group_by` | string | `"none"` | `"none"`, `"session"` or `"day"`. Inserts a `## <key>` header before each hit that starts a new group. Hit order and numbering do not change, so a group that is not contiguous gets its header again. `"session"`: the key is `session: <id>`, where the id is the source id for a fragment, else the hit's source session, else the hit falls back to its day. `"day"`: the `YYYY-MM-DD` of the hit's date tag, or `unknown date`. A header label is made single-line and capped at 80 characters (`…` beyond). Headers sit outside `max_context_tokens`: the budget trims hits before rendering and counts only hit and predecessor text. Any other value fails config loading |

---

## [resource_guards]

```toml
[resource_guards]
max_cpu_cores = 1.0
max_memory_mb = 512
min_disk_space_mb = 1024
llm_timeout_secs = 30
worker_timeout_secs = 120
```

Startup checks. Server refuses to start if resources are below thresholds.

---

## [streams] and [namespaces]

```toml
[streams]
shared = "001"

[streams.agents]
assistant = { raw = "100", compressed = "101" }

[namespaces]
personal = "100"
work = "110"
```

`streams` — agent-specific raw/compressed stream IDs.
`namespaces` — human-readable name → stream ID mapping. Returned by `memory_namespaces`.

Data written without an explicit stream goes to the built-in default stream `__user_default__`.

---

## [pii]

```toml
[pii]
enabled = true
redact_phones = true
redact_emails = true
redact_ids = true
blocklist_file = "pii_blocklist.txt"
audit_log = true
```

PII is stripped before **every** LLM API call (consolidation, extraction, dream). Original content in RocksDB remains untouched.

---

## [cost]

```toml
[cost]
daily_cap_usd = 15.00
alert_threshold_usd = 10.00
anomaly_multiplier = 3.0
persist = true
```

| Key | Type | Description |
|-----|------|-------------|
| `daily_cap_usd` | f64 | Hard stop — no LLM calls beyond this |
| `alert_threshold_usd` | f64 | Warning threshold |
| `anomaly_multiplier` | f64 | 3x typical daily cost = anomaly alert |
| `persist` | bool | Save cost counters across restarts |

---

## [knowledge_extraction]

```toml
[knowledge_extraction]
enabled = true
# model = "gpt-6-luna"  # unset: inherits [llm] compression_model
max_transcript_tokens = 20000
dedup_cosine_threshold = 0.92
contradiction_check = true
contradiction_cosine_min = 0.5
max_facts_per_chunk = 20
max_tokens = 16384
input_chunk_tokens = 4000
split_on_truncation = true
temporal_grounding = "off"
```

Controls the `memory_ingest` LLM pipeline.

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `max_facts_per_chunk` | int | 20 | Cap on admitted facts per transcript chunk (one planned input chunk, including its split retries), applied after the confidence filter; the prompt's "Maximum N facts per conversation" rule carries the same number per call. Every chunk is processed: a chunk's excess is counted in `discarded_facts` and never stops later chunks. Replaces `max_facts_per_transcript`, a cap on the whole transcript that stopped input processing once reached, so a long transcript lost everything past 20 facts. The old key is still accepted as an alias; a config with both keys is rejected |
| `max_tokens` | int | 16384 | Completion cap per extraction call, sent as `max_completion_tokens` through the `openai` entry (reasoning tokens share it). A reply that hits it (`finish_reason: "length"`) is rejected, never partially admitted |
| `input_chunk_tokens` | int | 4000 | Input budget per extraction call (4 chars ≈ 1 token): the transcript is split at line boundaries into chunks of at most `min(input_chunk_tokens, max_transcript_tokens)` tokens, one call each. `0` = one call per transcript up to `max_transcript_tokens` (the legacy pipeline), overlapping windows above it |
| `split_on_truncation` | bool | true | A chunk whose completion was truncated is re-extracted as two line-aligned halves (depth 2, at most 4 extra calls per chunk). `false` rejects and reports the truncated chunk, with no retry |

These are the shipped defaults since 2026-11-22 (an owner decision, deliberately without an A/B — the validation pilot is the measurement). Before, extraction was one call per transcript up to 20K tokens, with a 2000-token completion cap and 20 facts per transcript. The code defaults for a config that omits these keys stay at the legacy values (`20` / `2000` / `0` / `false`), so an older config behaves as before; with `input_chunk_tokens = 0` and `max_facts_per_transcript = 20` the whole transcript is one chunk and the per-chunk cap is the former cap, fact for fact. The one visible change for such a config: a transcript over `max_transcript_tokens`, which the legacy path processes as overlapping windows, now gets the cap per window. A truncated reply is never admitted in either mode; a completion that ends with `finish_reason = "length"` is an explicit error on every chat path, not only in extraction.

`temporal_grounding` is a quality lever (see [Quality levers](#quality-levers)):

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `temporal_grounding` | string | `"off"` | Rejects extracted facts whose relative time (such as "three weeks ago" or "yesterday") or `event_date_context` does not appear in the source chunk. `"off"`: no check; the prompt and the admitted facts are byte-identical to before. `"filter"`: drop such facts; the extraction prompt does not change. `"prompt_and_filter"`: `"filter"`, plus a TEMPORAL GROUNDING notice prepended to the extraction system prompt, which changes the prompt sent to the LLM. Any other value fails config loading |

The check compares whitespace-normalized, lowercased text, and it covers only some relative expressions: a fact that uses any other form passes the content check unexamined. Known forms it does not cover:

- English: "last night", "this morning", "next week", "in two weeks", "last Friday/weekend/summer", "a couple of weeks ago", "a few/several days ago", and number words above ten ("eleven days ago").
- Polish: "dziś", "przedwczoraj", "w zeszłym tygodniu", "tydzień/miesiąc/rok temu", and number words above five ("sześć dni temu" and up).

A build with the `evaluation` cargo feature reports the number of facts the check rejected as `grounding_rejected_facts` in the `memory_ingest` extraction receipt (`extraction_receipt.outcome`). The field is omitted while grounding is off, and regular builds carry no receipt.

---

## [fragments]

**Experimental, opt-in.** Off by default; existing deployments and clients keep byte-identical behavior.

```toml
[fragments]
enabled = false
max_fragment_tokens = 400
prefix_reserve_tokens = 16
provenance_role = "claim"
token_counter = "embedder"
```

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `enabled` | bool | false | Persist source fragments on `memory_ingest` |
| `max_fragment_tokens` | int | 400 | Token budget for a fragment's text, as measured by `token_counter`. Must be positive |
| `prefix_reserve_tokens` | int | 16 | Headroom above the text budget. The local embedder adds exactly two special tokens to the content (no `passage:` prefix is prepended); the rest absorbs the gap between the chosen counter and the embedder's own tokenizer |
| `provenance_role` | string | `"claim"` | Role stamped on every fragment chunk: `"claim"` (full weight), `"evidence"` (×0.5) or `"cue"` (×0.8). Any other value fails config load |
| `token_counter` | string | `"embedder"` | `"embedder"` (default and recommended: the loaded local embedding model's own tokenizer, the only counter whose budget is the embedder's), `"approx"` (`ceil(chars / 3)`, model-free) or `"tiktoken"` (`cl100k_base` BPE) — the two model-free counters are for non-local embedding providers and offline tests. Any other value fails config load |

When enabled, `memory_ingest` stores, right after the whole-conversation source and before extraction, one chunk per conversation turn (long turns are split further at paragraph, sentence and whitespace boundaries). Each fragment carries the author role, the conversation date and an exact byte range into the stored source. Fragments are `consolidated` from birth (never consolidation or dream input), are excluded from the `recent` section of `memory_context`, carry no event date of their own (their `memory_search` line shows the source's `observed:`/`recorded:` date), and are deleted together with their source. The tool result gains a trailing ` Stored N fragments.` sentence (see [MCP Tools](mcp-tools.md#memory_ingest)). Fragment writes are additive and never block fact extraction.

Validation at startup: `max_fragment_tokens` must be positive, and with `embedding_provider = "local"` the sum `max_fragment_tokens + prefix_reserve_tokens` must not exceed the local embedder's 512-token window. `enabled = true` with `token_counter = "embedder"` (the default) requires `embedding_provider = "local"` — with another provider, set `token_counter = "approx"` or `"tiktoken"` explicitly or config load fails. With `token_counter = "embedder"` and no local model loaded, segmentation fails and the `memory_ingest` text says so (there is no silent fallback to an approximation).

`provenance_role` affects ranking only on fused retrieval (vector-enabled, non-simple queries). The BM25-only path applies no provenance multiplier, so on short queries `claim` and `evidence` fragments score identically.

---

## [entity_extraction]

```toml
[entity_extraction]
enabled = true
# model = "gpt-6-luna"  # unset: inherits [llm] compression_model
batch_size = 10
flush_interval_secs = 3
queue_capacity = 200
confidence_threshold = 0.7
max_tokens_per_batch = 2000
```

Async LLM NER for entities not in the dictionary. `batch_size` is 10 since 2026-11-22 (was 20): with `reasoning_effort = "low"` a 14–15 chunk batch used 1696 of its 2000 output tokens, and 10 leaves headroom for the reasoning tokens that share the cap.

---

## [contradiction]

```toml
[contradiction]
enabled = true
similarity_threshold = 0.70
max_candidates = 5
# model = "gpt-6-luna"  # unset: inherits [llm] compression_model
```

When a new fact arrives, top `max_candidates` similar existing facts are checked via LLM for contradiction. Verdict: `updates` (supersede), `extends` (keep both), `none` (no relation).

---

## [conversation_extraction]

```toml
[conversation_extraction]
enabled = true
model = "gpt-4.1-mini"
max_tokens = 2000
confidence_threshold = 0.7
dedup_threshold = 0.80
max_extractions_per_request = 30
```

---

## [profile]

```toml
[profile]
enabled = true
# model = "gpt-6-luna"  # unset: inherits [llm] compression_model
max_chunks = 100
cache_ttl_secs = 3600
max_static_facts = 30
max_recent_items = 15
```

User profile synthesis. Cached for `cache_ttl_secs`.

---

## [retention]

```toml
[retention]
soft_delete_days = 30
hard_purge_interval_secs = 86400
```

| Key | Type | Description |
|-----|------|-------------|
| `soft_delete_days` | int | Days to keep soft-deleted chunks before hard purge |
| `hard_purge_interval_secs` | int | How often the purge worker runs (seconds) |

The hard-purge worker removes expired soft-deleted chunks from RocksDB, Tantivy, and graph.

---

## [associator]

```toml
[associator]
enabled = true
k_clusters = 0
max_clusters = 50
max_iterations = 100
min_serendipity = 0.1
max_associations = 3
```

| Key | Type | Description |
|-----|------|-------------|
| `enabled` | bool | Enable ECA serendipity engine and auto-clustering |
| `k_clusters` | int | Fixed cluster count (0 = auto) |
| `max_clusters` | int | Upper bound on auto-determined clusters |
| `min_serendipity` | f64 | Minimum score to include an association |
| `max_associations` | int | Max serendipitous results per search |

---

## [dream]

```toml
[dream]
enabled = true
batch_size = 50
min_group_size = 2
# model = "gpt-6-luna"  # unset: inherits [llm] compression_model
cost_cap_usd_per_run = 0.10
auto_trigger_threshold = 50
auto_cooldown_secs = 900
schedule_enabled = true
schedule_interval_secs = 86400
schedule_timeout_secs = 3600
```

| Key | Type | Description |
|-----|------|-------------|
| `batch_size` | int | Chunks per dream run |
| `min_group_size` | int | Min chunks per subject to merge |
| `cost_cap_usd_per_run` | f64 | Safety cap per dream session |
| `auto_trigger_threshold` | int | Fire a dream run after this many new chunks on a private stream (`0` = off; the code default is `0`, the shipped `config.toml` sets `50`) |
| `auto_cooldown_secs` | int | Minimum seconds between automatic runs on the same stream (default `900`) |
| `schedule_enabled` | bool | Run a periodic dream pass over every active stream. Code default `false` (a config written before this key existed stays on-demand); the shipped `config.toml` sets `true` |
| `schedule_interval_secs` | int | Seconds between scheduled passes (default `86400`, one day; allowed range `3600` to `2592000`) |
| `schedule_timeout_secs` | int | Soft deadline for the whole scheduled pass: no new stream is started after this many seconds, and the stream in progress finishes, and the next pass resumes at the first stream the deadline cut off (sorted order, wrapping) so later streams are not skipped forever (default `3600`, minimum `60`) |

The scheduled pass runs a dream over every active stream, shared streams included, and needs both `[dream] enabled = true` and `[scheduler] enabled = true`. The schedule is an instance-wide setting of a single-user engine, so one pass covers every stream. The dashboard Settings page (or `PUT /v1/dream/schedule`) can override `enabled` and the interval at runtime; the override is stored in the engine's database and survives restarts, while `schedule_enabled` and `schedule_interval_secs` stay the fallback when nothing is overridden. Enabling the schedule after it was off anchors the next run one interval from that moment, with no catch-up pass. A restart does not re-anchor: a run that became due while the engine was stopped runs once at the first poll after start (at most one catch-up, then the normal interval). All dream entry points (schedule, auto-trigger, `memory_dream`, `POST /v1/dream`) are mutually exclusive per store and stream: a run on a stream that is already being dreamed is skipped at once. `POST /v1/dream` reports `skipped_busy: true`, `memory_dream` replies `Dream skipped: another dream run is in progress on this stream.`, the scheduled pass counts such streams in `last_result.streams_skipped_busy`, and the auto-trigger only logs the skip. Manual `memory_dream` / `POST /v1/dream` and the count-based auto-trigger work independently of the schedule.

---

## [memory_generator]

```toml
[memory_generator]
enabled = true
max_chunks = 200
max_sections = 20
# model = "gpt-6-luna"  # unset: inherits [llm] compression_model
```

Controls `generate-memory-md` endpoint output.

---

## Environment variables

| Variable | Purpose | Required |
|----------|---------|----------|
| `LOOMEM_AUTH_TOKEN` | API Bearer key (name configurable via `server.auth_token_env`) | No (auth disabled if unset — local passthrough mode) |
| `OPENAI_API_KEY` | LLM API key (name configurable via `llm.api_key_env`, and per provider via `llm.providers.<name>.api_key_env`) | Only for a completions provider whose entry names it (the shipped `[llm.providers.openai]` does) or OpenAI embeddings (`embedding_provider = "openai"`). With local embeddings and no key, the default config starts only with `LOOMEM_ALLOW_KEYLESS=1` and the LLM steps are unavailable (`memory_store` and search still work). |
| `LOOMEM_CONFIG` | Config file path | No (default: `config.toml`) |
| `PORT` | Override server port | No |
| `SERVER_ORIGIN` | OAuth redirect base URL | No (for MCP Remote) |
| `LOOMEM_LOG_FORMAT` | `"json"` for structured logs | No |
| `LOOMEM_AT_REST_MASTER_KEY` | Master key enabling at-rest encryption (32-byte base64) | No |
| `LOOMEM_AT_REST_EXPECT_ENABLED` | Refuse to start without a master key when set | No |
| `LOOMEM_AMBIENT_CACHE_TTL_SECS` | Cache TTL for `/v1/ambient` responses | No (default 60) |
| `TELEGRAM_BOT_TOKEN`, `LOOMEM_TELEGRAM_CHAT_ID` | Optional cost-alert webhook | No |

For at-rest encryption, see [SECURITY.md — Data at rest](SECURITY.md#data-at-rest).
