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
surprise_boost = 1.5
synonyms_file = "synonyms.toml"
entities_file = "entities.toml"
rerank_enabled = false
rerank_candidates = 20
rerank_model_dir = "models/reranker"
multi_query_enabled = false
stem_polish = true
```

| Key | Type | Description |
|-----|------|-------------|
| `top_k` | int | Default number of search results |
| `surprise_boost` | f64 | Novelty multiplier on ingest (Titans-inspired) |
| `synonyms_file` | string | Path to synonym expansion map |
| `entities_file` | string | Path to entity dictionary |
| `rerank_enabled` | bool | Enable cross-encoder reranking (~97ms/pair) |
| `rerank_candidates` | int | Send top N to reranker |
| `rerank_model_dir` | string | ONNX model directory |
| `multi_query_enabled` | bool | Decompose complex queries into sub-queries |
| `stem_polish` | bool | Enable Polish language stemming |

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
provider = "openai"                  # completions (consolidation / reflect)
api_key_env = "OPENAI_API_KEY"
embedding_provider = "local"         # "local" (on-device ONNX) or "openai"
embedding_model = "text-embedding-3-small"   # used when embedding_provider = "openai"
# embedding_model_path = "/abs/path"  # local model dir; default ~/.loomem/models/multilingual-e5-small
embedding_dim = 384                  # MUST match the active embedding model
compression_model = "gpt-4.1-mini"
timeout_secs = 10
fallback_to_regex = true
```

| Key | Type | Description |
|-----|------|-------------|
| `provider` | string | Completions provider for consolidation / reflect (`"openai"`) |
| `embedding_provider` | string | `"local"` (on-device ONNX, no API key) or `"openai"`. Missing in older configs → treated as `"openai"` |
| `api_key_env` | string | Environment variable name for the API key |
| `embedding_model` | string | Embedding model name when `embedding_provider = "openai"` |
| `embedding_model_path` | string? | Directory with `model.onnx` + `tokenizer.json` for local embeddings. Unset → `~/.loomem/models/multilingual-e5-small` |
| `embedding_dim` | int | Embedding vector dimensions — **must match the active model** |
| `compression_model` | string | Model for consolidation, extraction, dream |
| `timeout_secs` | int | API call timeout |
| `fallback_to_regex` | bool | Use regex extraction if the completions LLM is unavailable |

### Local embeddings (default)

Fresh installs use **`embedding_provider = "local"`**: embeddings are computed
on-device with a quantization-free ONNX model (default
`multilingual-e5-small`, 384-dim, good multilingual/Polish recall) via the pure-Rust
`tract` runtime. No API key is required and no text leaves the machine. The model
ships in the default build; obtain the model files with:

```sh
./scripts/fetch-embedding-model.sh         # → ~/.loomem/models/multilingual-e5-small
```

The completions LLM (consolidation, fact extraction, dream) is independent: with
no `OPENAI_API_KEY`, those steps fall back to regex (`fallback_to_regex`), while
`memory_store` and semantic search work fully locally. To use a local LLM for
completions too, point `provider` at an OpenAI-compatible endpoint (future cycle).

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
model = "gpt-4.1-mini"
max_transcript_tokens = 20000
dedup_cosine_threshold = 0.92
contradiction_check = true
contradiction_cosine_min = 0.5
max_facts_per_transcript = 20
```

Controls the `memory_ingest` LLM pipeline.

---

## [entity_extraction]

```toml
[entity_extraction]
enabled = true
model = "gpt-4.1-mini"
batch_size = 20
flush_interval_secs = 3
queue_capacity = 200
confidence_threshold = 0.7
max_tokens_per_batch = 2000
```

Async LLM NER for entities not in the dictionary.

---

## [contradiction]

```toml
[contradiction]
enabled = true
similarity_threshold = 0.70
max_candidates = 5
model = "gpt-4.1-mini"
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
model = "gpt-4.1-mini"
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
model = "gpt-4.1-mini"
cost_cap_usd_per_run = 0.10
```

| Key | Type | Description |
|-----|------|-------------|
| `batch_size` | int | Chunks per dream run |
| `min_group_size` | int | Min chunks per subject to merge |
| `cost_cap_usd_per_run` | f64 | Safety cap per dream session |

---

## [memory_generator]

```toml
[memory_generator]
enabled = true
max_chunks = 200
max_sections = 20
model = "gpt-4.1-mini"
```

Controls `generate-memory-md` endpoint output.

---

## Environment variables

| Variable | Purpose | Required |
|----------|---------|----------|
| `LOOMEM_AUTH_TOKEN` | API Bearer key (name configurable via `server.auth_token_env`) | No (auth disabled if unset — local passthrough mode) |
| `OPENAI_API_KEY` | LLM API key (name configurable via `llm.api_key_env`) | Only for OpenAI completions (`provider = "openai"`) or OpenAI embeddings (`embedding_provider = "openai"`). With local embeddings (default) and no key, the LLM steps fall back to regex. |
| `LOOMEM_CONFIG` | Config file path | No (default: `config.toml`) |
| `PORT` | Override server port | No |
| `SERVER_ORIGIN` | OAuth redirect base URL | No (for MCP Remote) |
| `LOOMEM_LOG_FORMAT` | `"json"` for structured logs | No |
| `LOOMEM_AT_REST_MASTER_KEY` | Master key enabling at-rest encryption (32-byte base64) | No |
| `LOOMEM_AT_REST_EXPECT_ENABLED` | Refuse to start without a master key when set | No |
| `LOOMEM_AMBIENT_CACHE_TTL_SECS` | Cache TTL for `/v1/ambient` responses | No (default 60) |
| `TELEGRAM_BOT_TOKEN`, `LOOMEM_TELEGRAM_CHAT_ID` | Optional cost-alert webhook | No |

For at-rest encryption, see [SECURITY.md — Data at rest](SECURITY.md#data-at-rest).
