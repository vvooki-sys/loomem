# Memory routing — layers, query taxonomy, and scoring fusion

**Status:** design document. Describes how retrieval is routed across Loomem's memory layers. Implementation lives in `loomem-core/src/search/` (`query_taxonomy.rs`, `query_classifier.rs`, `fusion.rs`) and `loomem-core/src/ambient/`.

---

## 1. Philosophy: ranking/fusion, NOT a classifier

Routing in Loomem is a **ranking architecture, not a query classifier**. There is no LLM call that dispatches a query to a "vector engine" or a "graph engine". Instead, a deterministic pipeline runs **all retrieval signals in parallel for every query**, and the query type influences only the **weights in a late fusion step** — never which signals run.

Reasons for this decision:

1. **Latency** — an LLM classifier is an extra hop in the retrieval hot path.
2. **Cost** — per-query LLM classification is a linear cost on traffic; late fusion is pure arithmetic.
3. **Determinism** — a stochastic classifier makes retrieval traces unrepeatable; a deterministic ranking is debuggable.
4. **Composability** — in a fusion model, adding or removing a signal means changing a weight, not retraining a dispatcher.

A *light deterministic query parser* (regex, lightweight NER, time-marker detection) is allowed and used — but it returns a **weight vector**, not a "use this store" decision.

---

## 2. Memory layers

Each layer is a structurally distinct store (or view) with its own write semantics, storage, and retrieval signal. Layers are independent of each other.

| Layer | Stores | Storage | Written by | Retrieval signal |
|---|---|---|---|---|
| L0 chunks | raw ingested entries | RocksDB `chunk:L0:` + Tantivy + embedding | ingest API | dense, BM25 |
| L1 consolidates | consolidated facts (with supersede links) | RocksDB `chunk:L1:` + Tantivy + embedding | consolidation pipeline | dense, BM25 |
| Entity graph | named entities + relations | RocksDB `graph:entity:`, `graph:edge:` | entity extraction at ingest | entity match, edge traversal |

Two **cross-cutting dimensions** apply to all layers:

- **Stream partitioning** — every query carries a stream scope; every layer respects it as a hard filter, not a signal. The default stream is `__user_default__`.
- **Bitemporal model** — `valid_time` (when a fact was true) vs `decision_time` (when it was recorded). Acts as both filter and signal. Applies to chunks and the graph.

There is no higher abstraction tier above L1: an earlier L2 tier was removed, and clustering output now feeds the associator (serendipity engine) instead of a storage tier. (An earlier document-registry layer was removed with the file subsystem in cycle/005.)

---

## 3. Query type taxonomy

Five query types. Each has primary layers (the dominant signal source), secondary layers, and dominant fusion signals. Detection priority (first match wins): document_lookup > relational > temporal > recent > factual; the fallback default is **factual**.

| Query type | Example | Primary layers | Secondary | Dominant signals |
|---|---|---|---|---|
| **factual** | "what did I write about BLAKE3" | L0 + L1 | entity graph | dense + BM25 + entity match |
| **temporal** | "what happened in March" | L1 with time filter | L0 | dense + recency |
| **relational** | "who worked with Alice on the dashboard" | entity graph (edge traversal) | L0/L1 | entity match + graph traversal + dense |
| **recent** | "the last thing I mentioned" | L0 | — | recency (dominant) |
| **document_lookup** | "that paper I mentioned" | L0 + L1 | — | dense + BM25 |

Notes:

- **The mapping is not hard gating.** A factual query still queries the entity graph — just with lower weights. No layer is ever cut out entirely.
- **Classification is deterministic.** Temporal detection is regex on time markers ("yesterday", month names, dates); relational detection looks for multiple proper nouns plus a relational preposition; document lookup is triggered by explicit verbs ("I uploaded", "that file", "the PDF").
- Things that are deliberately *not* query types: multi-hop reasoning (a retrieval technique, not a type), code search, and summarization queries.

---

## 4. Scoring fusion

### 4.1 Signals

**Pre-fusion filters** (hard, applied before any signal computation):

- `scope_filter` — stream isolation; out-of-scope chunks never enter the ranking.
- `tombstone_filter` — superseded (`is_latest = false`) and tombstoned chunks are cut.

**Soft signals** (each normalized to [0,1], combined in fusion):

| Signal | What it computes | Source |
|---|---|---|
| `dense` | cosine(query embedding, chunk embedding) | embedding store |
| `lexical` | BM25 score | Tantivy index |
| `entity_match` | overlap between query entities and chunk entities | light NER + entity graph |
| `graph` | proximity bonus for chunks connected to query entities | entity graph |
| `recency` | exponential decay from the chunk's `decision_time` | timestamps |

### 4.2 Late fusion

Reciprocal Rank Fusion (RRF):

```
score_fused(c) = Σ_signal  w_signal · 1 / (k + rank_signal(c))
```

with `k = 60` (the standard literature default; `RRF_K` in `search/fusion.rs`) and `w_signal` a function of the query type.

### 4.3 Per-type weight tiers

Weights are expressed as tiers — **H** (dominant), **M** (contributes), **L** (present for regression safety), **—** (zero) — and normalized to sum to 1 per row:

| Type / Signal | dense | lexical | entity_match | graph | recency |
|---|---|---|---|---|---|
| factual | H | H | M | L | L |
| temporal | M | L | L | L | M |
| relational | M | L | H | H | L |
| recent | L | L | L | — | H |
| document_lookup | H | H | L | — | L |

### 4.4 Routing pipeline

```
1. Query in (text + scope)
2. Light parser (regex + lightweight NER, no LLM):
   - extract entities (proper nouns, file paths)
   - detect temporal markers
   - detect document-lookup verbs
   - classify type → fallback "factual"
3. Load weights for the type
4. Apply pre-fusion filters (scope, tombstones)
5. Run all soft signals in parallel across all layers
6. Renormalize weights over channels with non-empty results
   (cold-start safety: an empty graph doesn't waste weight)
7. RRF fusion with the renormalized weights
8. Top-k return + per-signal score breakdown per result (for debugging and eval)
```

**Hard rule: no LLM call in dispatch.** This applies to the decision of which signal runs with which weight — not to optional post-retrieval reranking (a cross-encoder rerank on the fused top-N is configurable via `[search] rerank_enabled`).

---

## 5. Delivery modes: ambient, verification, consolidation

The retrieval pipeline above can deliver memory to an agent in two modes, with a third background layer:

| Layer | Mechanism | Trigger | Notes |
|---|---|---|---|
| **Ambient** | `POST /v1/ambient` | per-turn deterministic injection by the agent host | Narrow retrieval (small top-k), recency-boosted, strict latency and token budgets. The agent does not decide whether to call memory — relevant facts are pre-loaded into its context. |
| **Verification** | `memory_search` MCP tool | agent decision, on demand | Full fusion pipeline, broader top-k. Used to verify or dig deeper when ambient context is insufficient. |
| **Consolidation** | dream/consolidation workers | background scheduler | L0 → L1 consolidation, contradiction resolution. |

The ambient and verification layers **share the same retrieval pipeline** (same fusion, same weights); they differ in delivery mode (push vs pull) and retrieval scope (narrow vs broad).

Rationale: agents are conservative about calling memory tools and about trusting tool results, so tool-only delivery loses recall in practice. Ambient injection removes the "should I call memory?" decision from the hot path. Internal evaluation of ambient delivery showed mixed results — strong gains on preference-style recall, no across-the-board improvement — so treat ambient injection as an optional integration, not a required default, and measure it against your own workload.

The full endpoint contract (wire shape, content-shape invariants, negative-result marker, caching, budgets) is documented in [the ambient endpoint contract](../../loomem-server/docs/ambient-endpoint.md).
