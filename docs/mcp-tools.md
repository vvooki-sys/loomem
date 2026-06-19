# MCP Tools Reference

Loomem exposes 14 `memory_*` tools via the Model Context Protocol. These tools are what Claude (or any MCP client) uses to interact with memory. The MCP server identifies itself as `loomem-memory`.

> **Runtime sync:** The condensed version sent to MCP clients lives in `loomem-server/mcp_instructions.md` (embedded at compile time). Edit that file to change what clients see.

---

## Scope model

Data lives in **streams**. Different tools operate at different scopes:

| Scope | Tools | What it means |
|-------|-------|---------------|
| **Per-stream** (isolated) | `memory_store`, `memory_search`, `memory_ingest`, `memory_context`, `memory_profile`, `memory_dream`, `memory_reflect`, `memory_associate` | Only sees/writes data in the active stream |
| **Global** (engine-wide) | `memory_status`, `memory_namespaces` | Returns system-level info |
| **Per-stream** (isolated graph) | `memory_graph` | Knowledge graph is per-stream isolated |
| **By chunk ID** | `memory_history`, `memory_delete`, `memory_feedback` | Operates on a specific chunk |

**Key concepts:**
- **Stream** — the data isolation boundary. The built-in default stream is `__user_default__`.
- **Namespace** — a human-readable label mapped to a stream ID (e.g., `personal` → `100`). Configured in `config.toml`, listed by `memory_namespaces`.

---

## Storing memories

### memory_store

Store a single, confirmed fact.

**Use for:** explicit user statements, confirmed decisions, biographical facts.
**Don't use for:** raw conversation text — use `memory_ingest` instead.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `content` | string | yes | The fact to store |
| `source` | string | no | Provenance (e.g., "user-stated") |
| `subject` | string | no | Entity name (person, project) |
| `metadata` | object | no | Custom JSON metadata |

**Example:**

```
memory_store(
  content: "Prefers Cursor over VSCode for AI features",
  subject: "Alice",
  source: "user-stated"
)
```

**Behavior:**
- Preferences automatically get `importance: 2.0`
- Entity extraction runs (dictionary + LLM queue)
- Contradiction detection checks against existing memories
- Embedding generated asynchronously

---

### memory_ingest

Extract structured knowledge from a conversation via LLM. **Preferred method** for storing multi-fact content.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `content` | string | yes | Conversation transcript |
| `conversation_date` | string | no | ISO date (e.g., "2026-04-02") |

**Example:**

```
memory_ingest(
  content: "User: I switched from VSCode to Cursor last week. Also, the dashboard deadline is April 15th.",
  conversation_date: "2026-04-02"
)
```

**What happens:**
1. LLM extracts individual facts with type, subject, date, confidence
2. Each fact checked for duplicates (cosine similarity > 0.92 = skip)
3. Contradiction detection runs (updates supersede older facts)
4. Facts stored with full `extraction_meta`

**Typical output (illustrative — exact extraction depends on LLM judgment):**
- `PreferenceOrDecision`: "User prefers Cursor over VSCode" (subject: user, confidence: 0.95)
- `ProjectState`: "Dashboard deadline is 2026-04-15" (subject: dashboard, confidence: 0.90)

The number of extracted facts, their phrasing, and confidence scores vary by input. Treat examples as representative, not contractual.

---

## Searching memories

### memory_search

Hybrid search across all memory tiers.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `query` | string | yes | Search query |
| `top_k` | int | no | Max results (default: 10) |
| `time_filter` | string | no | "today", "this_week", "this_month" |

**Example:**

```
memory_search(query: "What IDE does the user prefer?")
```

**Behavior:**
- Runs BM25 + vector + graph search in parallel
- Applies time decay (recent memories score higher)
- Deduplicates near-identical results
- Marks superseded facts with `[UPDATED]`
- For aggregation queries ("how many..."), boosts `top_k` to 30
- Each result line ends with its `chunk_id` — usable directly for `memory_history`, `memory_delete`, or `memory_feedback`

---

### memory_context

Load relevant background at the start of a task. Token-budgeted — won't overwhelm the context window.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `query` | string | no | Topic focus (e.g., "dashboard project") |
| `budget_tokens` | int | no | Max tokens (default: 4000) |
| `sections` | string[] | no | Which sections: `profile`, `relevant`, `recent` |

**Example:**

```
memory_context(query: "Loomem project", budget_tokens: 2000)
```

**Section allocation:**
- `profile` (20%) — who the user is, stable facts
- `relevant` (50%) — memories matching the query
- `recent` (30%) — last 7 days of activity

**When to call:** Once at conversation start, when the topic requires background knowledge.

---

### memory_profile

Get the user's synthesized profile.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `format` | string | no | `"markdown"` or `"json"` |

**Returns:**
- `stable_facts` — permanent truths (max 30)
- `recent_context` — last 7 days (max 15)
- `summary` — 2-3 sentence overview

Profile is LLM-generated and cached for 1 hour.

---

### memory_associate

Surface serendipitous associations — memories that are surprisingly relevant but not obvious — using graph walks, temporal co-occurrence, and semantic adjacency.

**Use for:** unexpected connections and creative links across topics (e.g., "what past projects relate to this new idea?"). Use `memory_search` for direct fact retrieval.

**Returns:** a numbered list of associations with mechanism and score. If results are empty, clustering may not have run yet — call `memory_dream` first.

---

## Memory maintenance

### memory_dream

Trigger memory consolidation. Like sleep for the brain — merges, deduplicates, and organizes memories.

**No parameters.**

**What happens:**
1. Groups memories by subject
2. Merges related observations into concise summaries
3. Resolves contradictions (newer facts win)
4. Creates L1 compressed chunks from L0 raw events
5. Respects cost cap ($0.10/run by default)

**Returns:**

```
{
  "chunks_processed": 50,
  "groups_found": 12,
  "facts_merged": 8,
  "contradictions_resolved": 2,
  "cost_usd": 0.04,
  "duration_ms": 3200
}
```

**When to call:**
- After a long, productive session
- When the user asks to "clean up" or "consolidate" memory
- Automatically triggered after 30 minutes of inactivity

---

### memory_reflect

Analyze memory quality and get improvement suggestions.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `max_chunks` | int | no | How many chunks to analyze |

**Returns:**

```
{
  "quality_score": 78,
  "breakdown": {
    "by_level": { "L0": 45, "L1": 62 },
    "with_extraction_meta": "87%",
    "with_subject": "72%"
  },
  "suggestions": [
    "28 chunks lack subject metadata — run memory_dream",
    "5 potential contradictions detected"
  ]
}
```

**Quality score formula:** `has_meta*0.4 + has_subject*0.3 + structured*0.3`

**When to call:** Periodically (once per week), or when memory quality seems poor.

---

### memory_graph

Explore entity connections in the knowledge graph. **Per-stream isolated** — each user sees only their own entities and connections.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `entity` | string | yes | Entity name to explore |

**Example:**

```
memory_graph(entity: "Cursor")
```

**Returns:** entity details (name, type, aliases), connections to other entities, linked chunk count. Only entities and edges within the active stream are visible.

Useful for understanding "who works on what", "what technologies are connected", etc.

---

### memory_history

Trace how a fact evolved over time (version chain).

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `chunk_id` | string | yes | Chunk to trace |
| `limit` | int | no | Max versions to return |

**Example:**

```
memory_history(chunk_id: "abc-123")
```

**Returns:** version chain showing how the fact changed:

```
v1 (2026-03-15): "User uses VSCode"        ← superseded
v2 (2026-04-01): "User uses Cursor"         ← current
```

**When to call:** when the user asks "what did I use before X?" or when investigating contradictory information.

---

### memory_delete

Delete a specific memory by ID.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `chunk_id` | string | yes | Memory to delete |

**Important:** Never delete without user confirmation.

---

### memory_feedback

Rate the usefulness of a memory chunk after a task, providing a reinforcement signal to the ranking system.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `chunk_id` | string | yes | Chunk to rate (from a `memory_search` result line, or the id returned by `memory_store` / `memory_ingest`) |
| `usefulness` | int | yes | 0 (not useful) – 4 (crucial) |
| `harmful` | bool | no | Set only when the chunk contained incorrect or misleading information |

Only rate chunks where the usefulness is clear; this is not a general write tool.

---

## Informational

### memory_status

Engine health and statistics. Use to verify the engine is running, check capacity, or diagnose issues when other tools seem unresponsive.

**No parameters.**

**When to call:**
- Tools return unexpected errors or empty results
- User asks about memory health or capacity
- Before a complex operation (bulk ingest, dream) to verify readiness

**Returns** a plain-text health block scoped to your stream, e.g.:

```
Loomem Status: ok
Your stream: __user_default__
Your memories: 142
Embeddings (this stream): warming up (118/142 indexed, 24 pending, 83%)
Associator: active (7 clusters)
Event log drops: 0
Audit write failures: 0
Undecodable chunks (last full scan): 0
LLM failures (last 60m): extraction=0, ner=0, embedding=0, consolidation=0
```

Key indicators:
- `Embeddings (this stream)` reads `ready` once every chunk is indexed; `warming up (... pending ...)` means vector search is still backfilling and falls back to BM25 until pending reaches 0.
- `Associator: enabled, awaiting clustering` means `memory_associate` has nothing to return until `memory_dream` runs.
- Non-zero `Event log drops`, `Audit write failures`, or `Undecodable chunks` signal data-integrity issues worth investigating.

(The JSON object with `uptime_secs` / `rocksdb_keys` / `vector` / `scheduler` is the HTTP admin stats endpoint — see the API reference — not this MCP tool.)

---

### memory_namespaces

List available memory spaces.

**No parameters.**

**Returns:**

```
{
  "namespaces": {
    "personal": "100",
    "work": "110"
  }
}
```

---

## Best practices

### Do

- Use `memory_ingest` for conversations (not `memory_store`)
- Call `memory_context` at conversation start for relevant tasks
- Run `memory_dream` after productive sessions
- Check `memory_reflect` periodically
- Use `memory_search` before answering from training data

### Don't

- Don't store raw conversation text via `memory_store`
- Don't call `memory_search` for every response — only when recall matters
- Don't store temporary status, small talk, or questions without answers
- Don't store passwords, API keys, or other secrets
- Don't delete without user confirmation
