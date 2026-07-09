# API Reference

Base URL: `http://localhost:3030` (or your deployment URL)

Authentication: `Authorization: Bearer <token>` header on all endpoints except `/health`. The token is the single API key configured via the env var named by `server.auth_token_env` (default `LOOMEM_AUTH_TOKEN`); if no key is configured the server runs in local passthrough mode and accepts all requests.

When no `stream` is specified, data is read from and written to the default stream `__user_default__`.

---

## Storage

### POST /v1/store

Store a memory chunk.

**Request:**

```json
{
  "content": "User prefers dark mode in all tools",
  "stream": "100",
  "level": 0,
  "metadata": { "source": "user-stated" },
  "user_id": "alice",
  "app_id": "claude"
}
```

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `content` | string | yes | — | Memory text (1 – 1M chars) |
| `stream` | string | no | `__user_default__` | Namespace ID |
| `level` | int | no | 0 | Memory tier (0 = raw, 1 = consolidated) |
| `metadata` | object | no | null | Custom JSON metadata |
| `user_id` | string | no | null | Creator identifier |
| `app_id` | string | no | null | Application identifier |

**Response:**

```json
{
  "id": "a1b2c3d4-e5f6-7890-abcd-ef1234567890",
  "status": "stored"
}
```

**Side effects:**
- Pre-ingestion sanitization (HTML stripping, instruction injection detection)
- PII redaction (phones, emails, PESEL, blocklist words)
- Entity extraction (dictionary + LLM queue)
- Embedding generation (async queue)
- Tantivy indexing
- Graph population (stream-scoped)
- Surprise scoring (importance adjustment)
- Contradiction detection against existing memories

LLM-based knowledge extraction from full conversation transcripts is available via the `memory_ingest` MCP tool — see [MCP Tools Reference](mcp-tools.md).

---

## Search

### POST /v1/search

Hybrid search across all memory tiers.

**Request:**

```json
{
  "query": "What IDE does the user prefer?",
  "top_k": 5,
  "stream": "100",
  "date_from": "2026-01-01",
  "date_to": "2026-04-03",
  "trace": true,
  "dry_run": false
}
```

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `query` | string | yes | — | Search query |
| `top_k` | int | no | 10 | Max results |
| `stream` | string | no | `__user_default__` | Namespace filter |
| `streams` | string[] | no | null | Multi-namespace search |
| `date_from` | string | no | null | ISO date lower bound |
| `date_to` | string | no | null | ISO date upper bound |
| `entity` | string | no | null | Filter by entity name |
| `trace` | bool | no | false | Include debug trace info |
| `dry_run` | bool | no | false | Skip implicit boost / access tracking |
| `valid_at` | int (unix sec) | no | null | Bitemporal time-travel: return only chunks whose `[valid_from, valid_until]` covers this timestamp. Open intervals (None) are unbounded on that side. |
| `include_superseded` | bool | no | false | Include old versions |
| `fact_type` | string | no | null | Filter: `preference`, `project`, `fact` |
| `subject` | string | no | null | Filter by subject entity |
| `min_confidence` | f64 | no | null | Minimum extraction confidence |

**Response:**

```json
{
  "results": [
    {
      "chunk_id": "abc-123",
      "content": "User switched from VSCode to Cursor (reason: speed)",
      "score_final": 0.87,
      "trace_info": {
        "level": "L1",
        "source": "consolidation",
        "is_latest": true,
        "created_at": 1743638400,
        "memory_type": "static",
        "importance": 1.2,
        "access_count": 3,
        "version": 2,
        "superseded_by": null
      }
    }
  ],
  "trace_metadata": {
    "total_results_before_topk": 42,
    "dedup_removed": 3,
    "search_latency_us": 1200,
    "query_complexity": "medium"
  }
}
```

---

### POST /v1/context-pack

Smart context packing for system prompt injection. Assembles a token-budgeted context window from profile, relevant memories, and recent activity.

**Request:**

```json
{
  "query": "working on dashboard project",
  "stream": "100",
  "budget_tokens": 2000,
  "sections": ["profile", "relevant", "recent"]
}
```

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `query` | string | null | Topic focus |
| `stream` | string | `__user_default__` | Namespace |
| `budget_tokens` | int | 4000 | Max tokens in response |
| `sections` | string[] | all | Which sections to include |

**Section token allocation:**
- `profile`: 20% of budget
- `relevant`: 50% of budget
- `recent`: 30% of budget

**Response:**

```json
{
  "context": "## Profile\nUser is a software engineer...\n\n## Relevant\n...\n\n## Recent\n...",
  "sources": [
    { "chunk_id": "abc-123", "score": 0.87, "section": "relevant" }
  ],
  "total_tokens": 1856,
  "sections_included": ["profile", "relevant", "recent"],
  "sections_truncated": []
}
```

---

## Memory management

### POST /v1/delete

Delete a single chunk by ID.

```json
{ "id": "abc-123" }
```

Response: `{ "status": "deleted", "id": "abc-123" }`

### DELETE /api/memories/{id}

REST-compliant delete. Optional `?ns=<namespace>` query parameter.

Response: `{ "deleted": true, "id": "abc-123" }`

---

### POST /v1/purge-namespace

Delete all chunks in a stream.

```json
{
  "stream": "100",
  "dry_run": true,
  "confirmed": false
}
```

Response:

```json
{
  "status": "ok",
  "stream": "100",
  "dry_run": true,
  "deleted_count": 42,
  "deleted_ids": ["abc-123", "def-456", "..."]
}
```

Set `dry_run: false` and `confirmed: true` to actually delete.

---

## Graph

### GET /v1/graph/entity/{name}?stream={stream_id}

Get entity node with edges and chunk references. **Requires `stream` parameter** — graph is per-stream isolated.

```json
{
  "entity": {
    "id": "ent-123",
    "name": "Cursor",
    "type": "TECHNOLOGY",
    "aliases": ["cursor IDE"],
    "chunk_count": 2
  },
  "neighbors": [
    { "entity": "Alice", "entity_type": "PERSON", "relation": "uses", "direction": "incoming" }
  ],
  "chunk_ids": ["abc-123", "def-456"]
}
```

### GET /v1/graph/stats

```json
{
  "total_entities": 42,
  "total_edges": 87,
  "avg_chunks_per_entity": 3.2
}
```

### POST /v1/build-graph

Rebuild entity graph from all stored entities. Useful after bulk ingestion.

### POST /v1/extract-entities

Trigger LLM NER backfill on chunks missing entity extraction. Runs asynchronously.

---

## Synthesis

Version history of a chunk is available via the `memory_history` MCP tool; the synthesized user profile via the `memory_profile` MCP tool. See [MCP Tools Reference](mcp-tools.md).

### GET /v1/generate-memory-md

Generate a MEMORY.md proposal from top chunks.

```json
{
  "proposal": "# Memory\n\n## Identity\n- Software engineer...\n\n## Preferences\n...",
  "metadata": { "chunks_considered": 200, "sections": 15 }
}
```

---

## Consolidation & maintenance

### POST /v1/dream

Trigger dream consolidation on current stream.

```json
{
  "stream": "100",
  "chunks_processed": 50,
  "groups_found": 12,
  "facts_merged": 8,
  "contradictions_resolved": 2,
  "cost_usd": 0.04,
  "duration_ms": 3200
}
```

Memory quality analysis is available via the `memory_reflect` MCP tool.

### POST /v1/boost

Boost a chunk's importance to 1.5.

```json
{ "id": "abc-123" }
```

### POST /v1/embed-missing

Backfill embeddings for chunks that don't have them.

```json
{
  "status": "ok",
  "total_missing": 15,
  "embedded": 15,
  "failed": 0
}
```

### POST /v1/retag-all

Re-extract entities on all chunks.

### POST /v1/score-all

Recompute importance scores for all chunks via embedding similarity.

### POST /v1/re-embed-all

Regenerate all embeddings (useful when switching providers).

### POST /v1/reset-importance

Reset all accumulated implicit boosts back to defaults.

### POST /v1/reset-backfill

Clear LLM entity extraction markers to allow re-processing.

---

## Admin

### GET /v1/status

Engine health and statistics.

```json
{
  "status": "ok",
  "uptime_secs": 86400,
  "config_summary": {
    "vector_enabled": true,
    "tantivy_enabled": true,
    "scheduler_enabled": true,
    "rocksdb_keys": 311,
    "tantivy_docs": 115,
    "embeddings_count": 112
  }
}
```

### GET /v1/namespaces

```json
{
  "namespaces": {
    "personal": "100",
    "work": "110"
  }
}
```

### GET /v1/whoami

Returns the auth context for the current caller (active stream, role, accessible streams).

### GET /health

Liveness check. **No authentication required.**

```json
{ "status": "ok" }
```

---

## Ambient memory

`POST /v1/ambient` returns a small, token-budgeted set of plain-fact memory snippets for injection into an agent's context at the start of a turn. See [the ambient endpoint contract](../loomem-server/docs/ambient-endpoint.md).

---

## Background workers

### POST /admin/workers/pause

Pause all background workers (consolidation, decay, compaction, backup, clustering, purge, stats). Useful for eval runs where you need a frozen database.

**Requires admin token.**

**Response:**

```json
{
  "paused": true,
  "message": "All workers paused"
}
```

### POST /admin/workers/resume

Resume all background workers.

**Response:**

```json
{
  "paused": false,
  "message": "All workers resumed"
}
```

### GET /admin/workers/status

Check if workers are paused.

**Response:**

```json
{
  "paused": false
}
```

Pause state does not survive server restart — workers resume automatically on startup.

---

## MCP endpoint

### POST /mcp

MCP JSON-RPC 2.0 endpoint. Used by Claude Desktop, claude.ai connectors, and other MCP clients. The server identifies itself as `loomem-memory`.

See [MCP Tools Reference](mcp-tools.md) for the 15 available `memory_*` tools.

### OAuth endpoints

For MCP Remote Connector authorization:

```
POST  /oauth/register      Dynamic Client Registration (RFC 7591)
GET   /oauth/authorize      Authorization page (user enters API key)
POST  /oauth/authorize      Submit authorization
POST  /oauth/token          Exchange code for access token
```
