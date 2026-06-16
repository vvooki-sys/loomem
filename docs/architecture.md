# Architecture

## Overview

Loomem is a Rust workspace with four crates:

| Crate | Role |
|-------|------|
| `loomem-core` | Core library — storage, search, consolidation, graph, LLM, embeddings |
| `loomem-server` | HTTP + MCP server (Axum), auth, handlers, routing |
| `loomem-cli` | Command-line interface for direct interaction |
| `loomem-migrate` | Data migration utilities |

All business logic lives in `loomem-core`. The server is a thin HTTP layer that maps routes to core functions.

---

## Data model

### Chunk

The fundamental unit of memory. Every piece of knowledge — raw event, compressed summary, semantic group — is a Chunk.

```
chunk:L{level}:{uuid}  →  Chunk (JSON in RocksDB)
```

**Core fields:**

| Field | Type | Description |
|-------|------|-------------|
| `id` | UUID | Unique identifier |
| `content` | String | Memory text |
| `stream` | String | Namespace / isolation boundary (default: `__user_default__`) |
| `level` | 0, 1 | Memory tier (raw, compressed) |
| `score` | f64 | Decay-adjusted relevance (0.0 – 1.0) |
| `timestamp` | u64 | Ingestion time (Unix seconds) |
| `importance` | f64 | Surprise-based boost (0.7 – 1.5) |
| `access_count` | u32 | Search hit counter (for adaptive decay) |
| `persistent` | bool | Exempt from decay |
| `is_latest` | bool | Head of supersede chain |

**Consolidation fields:**

| Field | Type | Description |
|-------|------|-------------|
| `consolidated` | bool | L0: has been compressed to L1 |
| `source_ids` | Vec\<String\> | L1: which L0 chunks were merged |
| `prompt_version` | u32 | Which consolidation prompt was used |

**Contradiction / versioning fields:**

| Field | Type | Description |
|-------|------|-------------|
| `superseded_by` | Option\<String\> | Points to newer version |
| `supersedes_id` | Option\<String\> | What this chunk replaced |
| `root_memory_id` | Option\<String\> | Root of the version chain |
| `version` | u32 | Semantic version counter |

**Knowledge extraction metadata:**

| Field | Type | Description |
|-------|------|-------------|
| `extraction_meta.fact_type` | Enum | `PreferenceOrDecision`, `ProjectState`, `Fact` |
| `extraction_meta.subject` | String | Entity name (person, project) |
| `extraction_meta.event_date` | String | ISO date of the event |
| `extraction_meta.confidence` | f64 | LLM extraction confidence (0.0 – 1.0) |

### Memory tiers

```
L0 (Raw)          L1 (Compressed)
─────────         ─────────────
Verbatim input    LLM-summarized
Fast decay        Medium decay
```

**Promotion flow:**

```
Ingest → L0 (raw event)
            │
            ▼  [Consolidation worker, every 5 min]
         L1 (compressed observations)
```

A separate clustering worker periodically groups L1 chunks by embedding similarity; the clusters feed the associator (serendipity engine), not a separate storage tier.

### Entity graph

Entities and their relationships form a knowledge graph stored in RocksDB:

```
EntityNode {
    canonical_name: "Cursor"
    entity_type: "TECHNOLOGY"
    aliases: ["cursor", "Cursor IDE"]
    chunk_ids: ["abc-123", "def-456"]   // evidence
}

Edge {
    source: "Alice"
    target: "Cursor"
    relation_type: "uses"
    chunk_ids: ["abc-123"]              // evidence
}
```

**Per-stream isolation:** Each entity and edge is scoped to a `stream_id`. The same entity name in two streams gets separate entity nodes. Name/alias indexes are prefixed by stream.

**RocksDB key scheme for graph:**

```
graph:entity:{id}                    → EntityNode (JSON, includes stream_id)
graph:s:{stream_id}:name:{lower()}   → entity_id
graph:s:{stream_id}:alias:{lower()}  → entity_id
graph:adj:{src_id}:{edge_id}         → target_id
graph:radj:{tgt_id}:{edge_id}        → source_id
graph:chunk:{chunk_id}               → [entity_ids]  (reverse index)
```

---

## Storage layer

### RocksDB

Primary persistent store. Holds chunks, embeddings, graph, users, cost tracking.

**Column families:**

| Family | Contents |
|--------|----------|
| default | Chunks (`chunk:L{n}:{id}`), graph |
| embeddings | Vectors (`emb:{id}` → f32 array) |
| cost | Daily cost counters (`cost:{date}`) |
| keys | Wrapped per-stream data-encryption keys (at-rest encryption) |

**Configuration:**
- Compression: LZ4
- Write buffer: 64 MB x 3 buffers
- Max open files: 1000

### Tantivy

Full-text search index. Mirrors chunk content with additional indexed fields.

**Schema:**

| Field | Type | Boost | Purpose |
|-------|------|-------|---------|
| `id` | String | — | Chunk reference |
| `content` | Text | 1.0 | Primary search target |
| `entities` | Text | 0.2 | Entity mentions (comma-separated) |
| `relations` | Text | 0.2 | Relation triples |
| `stream` | String | — | Filtering |
| `timestamp` | i64 | — | Date filtering |
| `event_date` | i64 | — | Temporal queries |
| `level` | i64 | — | Tier filtering |

**Polish stemming** enabled for content field.

### Vector store

Embeddings stored in RocksDB's embeddings column family.

**Providers:**
- **OpenAI** — `text-embedding-3-small` (1536 dimensions)
- **Local** — tract ONNX runtime (pure Rust, no native dependencies)

**Embedding queue:** batch processing (50 items, 5s flush interval) to amortize API latency.

### Intent log (WAL)

Write-ahead log ensures cross-store consistency between RocksDB and Tantivy.

```
1. Append: PENDING(op_type, chunk_id)
2. Write to RocksDB
3. Write to Tantivy
4. Append: COMMITTED(op_type, chunk_id)
```

On crash recovery:
- PENDING without COMMITTED → rollback partial writes
- COMMITTED → verify both stores have the data

---

## Ingest pipeline

Content is sanitized before any storage:

```
Input content
    │
    ▼  [sanitizer.rs]
 1. HTML tag stripping + entity decoding
 2. Instruction injection detection (18 patterns — logs warning, does not block)
    │
    ▼  [pii_filter.rs]
 3. PII redaction (phones, emails, PESEL, blocklist words)
    │
    ▼  [persist_chunk]
 4. RocksDB store (sanitized content)
 5. Entity extraction + graph population (stream-scoped)
 6. Tantivy indexing
 7. Embedding queue
 8. Contradiction detection
```

The sanitizer detects but does not block injection attempts — content is stored after stripping. PII redaction replaces sensitive data with `[PHONE]`, `[EMAIL]`, `[ID]`, `[REDACTED]` tokens before storage.

---

## Search pipeline

A query flows through multiple stages:

### 1. Query classification

```
"what do you know about me?"   → Profile
"how many projects do I run?"  → Aggregation (top_k boosted to 30)
"when did I change my IDE?"    → Temporal (date filtering)
"why did I choose Rust?"       → Complex (top_k = 20)
"my dog's name"                → Simple (top_k = 3, BM25 only)
```

### 2. Date filter extraction

Parses relative dates from query text:
- "last week" → `date_from: now - 7d`
- "in March" → `date_from: 2026-03-01, date_to: 2026-03-31`
- Explicit `date_from`/`date_to` params override

### 3. Parallel search

BM25 and vector search run concurrently:

**BM25 (Tantivy):**
```
QueryParser(content^1.0, entities^0.2, relations^0.2)
  + stream filter
  + date range filter
  → ranked results
```

**Vector (cosine similarity):**
```
query_embedding = embed(query_text)
for each stored embedding:
    score = cosine(query_embedding, chunk_embedding)
→ top-K by similarity
```

### 4. Fusion

```
normalized_bm25   = bm25_score / max_bm25
normalized_vector  = vector_score / max_vector
fusion_score       = 0.6 * normalized_vector + 0.4 * normalized_bm25
```

### 5. Time decay

```
age_days = (now - chunk.timestamp) / 86400
lambda   = { L0: search.decay.l0_lambda, L1: search.decay.l1_lambda }
decay    = e^(-lambda * age_days)
score    = fusion_score * decay
```

### 6. Graph enhancement

For top results, find related entities and add their connected chunks:

```
result "Cursor" → entity "Cursor" → edges → related entities
  → neighbor chunks added with score * boost_factor (0.3)
```

### 7. Deduplication

Collapse near-identical results (high cosine similarity between result contents).

### 8. Optional reranking

If enabled, top-20 candidates are re-scored by:
- ONNX cross-encoder (local, ~97ms/pair) — or
- Async LLM reranking with speculative cache

### 9. Implicit boost

Non-dry-run searches increment `access_count` and boost `importance` of returned chunks (capped at 1.5, 1-hour cooldown).

### 10. Response

```json
{
  "results": [
    {
      "chunk_id": "abc-123",
      "content": "User prefers Cursor over VSCode",
      "score_final": 0.87,
      "trace_info": {
        "level": "L1",
        "source": "consolidation",
        "is_latest": true,
        "access_count": 5
      }
    }
  ],
  "trace_metadata": {
    "total_results_before_topk": 42,
    "search_latency_us": 1200
  }
}
```

---

## Background workers

The scheduler orchestrates background jobs. All workers can be paused/resumed at runtime via `POST /admin/workers/pause|resume` (useful for eval runs). Pause state is an `AtomicBool` shared between `AppState` and `Scheduler` — does not survive restart.

### Consolidation (L0 → L1)

| Setting | Default |
|---------|---------|
| Interval | 5 minutes |
| Batch size | 200 chunks |
| Min age | 60 seconds |
| Min chunks | 3 per stream |
| Style | `observation` (granular facts) |
| Similarity threshold | 0.3 (cosine, for topic grouping) |

**Process:**
1. Scan unconsolidated L0 chunks
2. Group by stream (user isolation)
3. **Sub-group by topic similarity** — greedy clustering on embeddings (cosine threshold). Prevents unrelated facts from merging into one L1 chunk.
4. PII redaction (per sub-group)
5. LLM compression (gpt-4.1-mini) — one call per sub-group
6. Create L1 chunk with `source_ids` linking back to L0
7. Index in Tantivy + embedding queue
8. Mark L0 as `consolidated: true`

Chunks without embeddings fall back to a single group (pre-clustering behavior). Single-topic streams produce one sub-group — zero regression risk.

### Decay

| Setting | Default |
|---------|---------|
| Interval | 1 hour |
| L0 factor | 0.990 per hour |
| L1 factor | 0.995 per hour |
| Dormant threshold | 0.01 |
| Adaptive | enabled (ACT-R) |

**Adaptive decay:** chunks with high `access_count` decay slower (`adaptive_dampening` / `adaptive_cap` in `[worker.decay_worker]`).

### Clustering

| Setting | Default |
|---------|---------|
| Interval | 6 hours |
| Algorithm | k-means on L1 embeddings |
| Max iterations | 1000 |

Cluster output feeds the associator (below); there is no separate storage tier for clusters.

### Entity extraction queue

| Setting | Default |
|---------|---------|
| Flush interval | 3 seconds |
| Queue capacity | 200 |
| Confidence threshold | 0.7 |
| Model | gpt-4.1-mini |

Async LLM NER runs in background for entities not in the dictionary.

### Embedding queue

| Setting | Default |
|---------|---------|
| Batch size | 50 |
| Flush interval | 5 seconds |

### Associator (ECA — serendipity engine)

| Setting | Default |
|---------|---------|
| Interval | 6 hours (with clustering) |
| Min serendipity | 0.1 |
| Max associations | 3 per query |
| Mechanisms | graph walk, temporal, adjacent |

**Components:**
- **Clustering** — k-means on chunk embeddings, per-stream
- **Graph walk** — random walk with weak-tie preference (fewer shared chunks = more novel)
- **Temporal** — find chunks near the same time period
- **Serendipity scoring** — relevance × (1 - obviousness) × cluster distance

### Hard-purge (retention worker)

| Setting | Default |
|---------|---------|
| Interval | 24 hours (`retention.hard_purge_interval_secs`) |
| Retention window | 30 days (`retention.soft_delete_days`) |

Scans for soft-deleted chunks past their recovery window. Purge pipeline: graph references → Tantivy → RocksDB hard delete (chunk + embedding + entities + relations).

### Dream (auto-consolidation)

| Setting | Default |
|---------|---------|
| Auto-trigger | 30 min idle |
| Batch size | 50 chunks |
| Min group size | 2 |
| Cost cap | $0.10 / run |

---

## Authentication

Loomem is single-user: one API key controls access to the whole instance.

- The key is read from the env var named by `server.auth_token_env` in `config.toml` (default `LOOMEM_AUTH_TOKEN`).
- All requests require `Authorization: Bearer <key>`; `/health` remains open.
- If no key is configured, the server runs in **local passthrough mode** — every request is accepted with admin privileges. Use only for local development.

Data written without an explicit stream lands in the default stream `__user_default__`. Additional streams (from `[streams]` / `[namespaces]` in config) partition data within the same instance — they are an organizational boundary, not separate identities.

### OAuth 2.0

For MCP Remote Connector (claude.ai):
- Dynamic Client Registration (RFC 7591)
- Authorization Code flow with PKCE
- User enters API key during authorization
- Access token = API key (no extra token layer)

---

## MCP integration

Loomem implements the Model Context Protocol (MCP) as a JSON-RPC 2.0 endpoint at `POST /mcp`.

**Request flow:**

```
Claude (MCP client)
    │
    ▼
POST /mcp (JSON-RPC)
    │
    ▼
mcp::handler → parse request → extract tool + args
    │
    ▼
mcp::dispatcher → match tool name → call internal handler
    │
    ▼
loomem-core / handlers → execute → return ToolResult
    │
    ▼
JSON-RPC response → Claude
```

**Session management:** OAuth tokens map to sessions, sessions map to `stream_id` for data isolation.

---

## Crash recovery

1. **Intent log replay** — on startup, scan WAL for uncommitted operations
2. **Partial write detection** — check RocksDB and Tantivy for consistency
3. **Orphan cleanup** — remove chunks marked `in_progress` from failed consolidation
4. **Tantivy rebuild** — if schema version mismatch, rebuild index from RocksDB source of truth

---

## Cost tracking

Every LLM call (consolidation, extraction, embedding) is tracked:

```
[cost]
daily_cap_usd = 15.00           # Hard stop
alert_threshold_usd = 10.00     # Warning
anomaly_multiplier = 3.0        # 3x typical = anomaly alert
```

Costs persisted in RocksDB column family. Workers check budget before each LLM call.
