# `POST /v1/ambient` — ambient memory endpoint

**Scope:** deterministic synthesis, in-memory cache, single-instance.

`/v1/ambient` is the **ambient delivery layer** of the three-layer model described in `docs/architecture/memory-routing.md` §5 (ambient / verification / consolidation). This document is the contract that any agent host integrating ambient memory MUST honor. The content shape — declarative plain-fact `text`, tier+score schema, `debug` server-side-only — is **invariant** and not per-host negotiable.

---

## 1. Purpose

When an agent receives a user message, the `memory_search` MCP tool gives it the *option* to query memory before answering. In practice agents frequently abstain from calling the tool, and treat tool results with trained skepticism, even when retrieval would have produced the answer.

`/v1/ambient` flips the framing: **memory arrives in the agent's context as a fact**, not as a tool result. The agent doesn't decide whether to trust it; it just reads it. Verification is reactive (the `memory_search` tool), not the default path.

The constraints below are what make that work: cited phrasing ("according to your memory…") re-engages the agent's skepticism. The plain-fact constraint is load-bearing, not stylistic.

---

## 2. Wire shape

### 2.1 Request

```http
POST /v1/ambient
Authorization: Bearer <token>
Content-Type: application/json

{
  "user_id": "user_42",
  "scope": "private:user_42",
  "recent_turns": [
    {"role": "user", "content": "What's that playlist I made?"},
    {"role": "assistant", "content": "I'll check..."}
  ],
  "hint": null,
  "refresh": false,
  "suppress_negative_marker": false
}
```

| Field | Type | Required | Notes |
|---|---|---|---|
| `user_id` | string | yes | Caller's logical id (used in cache key + telemetry). |
| `scope` | string | yes | Either a stream id (`__user_default__`) or a prefixed form (`private:__user_default__`). The trailing `:<id>` portion is matched against the caller's stream memberships. |
| `recent_turns` | array | no | Up to N most-recent conversation turns (host's call). Drives query derivation if `hint` absent. |
| `hint` | string | no | Explicit retrieval query string. Takes priority over `recent_turns`. |
| `refresh` | bool | no | When `true`, bypass the cache for this call AND skip seeding the cache from the response. Default `false`. |
| `suppress_negative_marker` | bool | no | When `true`, suppress the negative-ambient marker even when one would be emitted. See §4.2. |

### 2.2 Response (HTTP 200)

```json
{
  "snippets": [
    {"text": "User's Spotify playlist is named 'Summer Vibes'.", "tier": "high", "score": 0.82},
    {"text": "User shops at Target every other week.", "tier": "high", "score": 0.78}
  ],
  "marker_if_empty": null,
  "debug": {
    "trace_ids": ["chunk_4f3a...", "chunk_8e1c..."],
    "signal_breakdown_per_snippet": [...],
    "latency_ms": {"retrieval": 23, "synthesis": 4, "total": 31}
  }
}
```

| Field | Type | Notes |
|---|---|---|
| `snippets` | array | Up to 5 ranked plain-fact snippets. May be empty. |
| `snippets[].text` | string | Declarative plain-fact (see §3 invariants). |
| `snippets[].tier` | enum | `"high"`, `"medium"`, `"low"`, `"conflict"`. |
| `snippets[].score` | float | `[0,1]` continuous internal confidence. |
| `marker_if_empty` | object\|null | Negative ambient marker (see §4). |
| `debug` | object | Server-side telemetry — see §5 — **MUST NOT** be injected into the agent. |

The `score` aggregates the fused retrieval score with provenance (L0 direct statement vs L1 consolidated), recency, and cross-chunk corroboration. The `tier` is the external surface for the agent (easy to map to prescribed behavior); the continuous score is for calibration and debugging.

### 2.3 Failure mode (HTTP 200 with `marker_if_empty.reason`)

Storage / synthesis / timeout failures do **not** return 5xx. The response is HTTP 200 with `snippets: []` and a `marker_if_empty` that carries a structured `reason`:

```json
{
  "snippets": [],
  "marker_if_empty": {
    "ambient": "no_relevant_context",
    "checked": true,
    "scope": "private:user_42",
    "reason": "degraded_retrieval"
  },
  "debug": {
    "trace_ids": [],
    "signal_breakdown_per_snippet": [],
    "latency_ms": {"retrieval": 0, "synthesis": 0, "total": 7},
    "error": "RocksDB: storage offline"
  }
}
```

The agent host SHOULD treat this as a graceful fallback signal — the `memory_search` tool can be invoked instead. HTTP 4xx / 5xx responses are reserved for: 401 (no auth), 403 (scope mismatch — see §6), 400 (malformed request).

---

## 3. INVARIANT content-shape constraints

These are **not per-host negotiable**. Every agent host wiring `/v1/ambient` MUST honor them.

### 3.1 Plain-fact `text` only

Allowed (declarative, 3rd-person, no provenance citation):

> ✅ `User's Spotify playlist is named 'Summer Vibes'.`
> ✅ `User redeemed a $5 coupon on coffee creamer at Target.`
> ✅ `User's daily commute takes 45 minutes each way.`

Forbidden (cited / metaword / 1st-person):

> ❌ `According to your memory, the playlist is 'Summer Vibes'.`
> ❌ `Based on what I recall, the user's playlist…`
> ❌ `From your notes, the playlist is…`
> ❌ `I remember that the playlist is…`
> ❌ `Your memory says the playlist is named…`
> ❌ `Your Spotify playlist is named…` (2nd person — weaker than 3rd)

The server enforces this at synthesis time via a case-insensitive forbidden-pattern match. Cited synthesis attempts are dropped fail-closed rather than emitted. **Hosts MUST NOT post-process snippets to add metawords or citations.**

### 3.2 Schema invariant: `{text, tier, score}` always together

Score-only delivery (no tier label) is not allowed; tier without score is also not allowed. The triple is required because the host's prompt template prescribes per-tier behavior (commit / hedge / drop / disambiguate) — the LLM cannot differentiate without an explicit tier label, and the score is needed for downstream calibration and debugging.

A reasonable per-tier prescription for the host's prompt template:

```
If tier=high:   state the fact directly without citing memory as source.
If tier=medium: state the answer with one epistemic softening phrase
                ("I think", "I believe") — still without citing memory.
If tier=low:    do NOT use the snippet; rely on memory_search if relevant.
If tier=conflict: ask the user to disambiguate before committing.
If marker_if_empty is set: do NOT pretend to remember anything;
                call memory_search if the user expects recall.
```

### 3.3 No metawords in `text`

Forbidden phrase patterns (regex-matched server-side, fail-closed):

- `(according to|based on|from) (your|my|the) (memory|notes|records|background)`
- `i (remember|recall) (that|seeing|reading)`
- `(your|my) memory (says|shows|indicates|tells|contains)`

If the underlying chunk content matches any of these and the synthesizer cannot rewrite it cleanly, the snippet is silently dropped from the response. An empty snippet list with `marker_if_empty.reason: "all_low_tier"` or `"below_threshold"` is the expected fallback.

### 3.4 `debug` is server-side only — NEVER inject into agent context

```text
agent-facing payload  =  {snippets, marker_if_empty}    ✅ inject this
ops/eval-only         =  debug.{trace_ids,
                                 signal_breakdown_per_snippet,
                                 latency_ms,
                                 error}                  ❌ NEVER inject
```

`debug` carries provenance metadata (chunk IDs, source dates, per-signal scores). Injecting it re-introduces the cited-form skepticism that ambient delivery exists to avoid. Hosts MUST strip `debug` before constructing the system prompt or whatever surface they use to deliver ambient.

### 3.5 Delivery surface

Hosts SHOULD deliver snippets via the **system-prompt slot**. When the host platform locks the system prompt, a synthesized tool-result envelope is an acceptable fallback. Bracketed prepends inside the user message (`[Background memory: …]`) are **not supported** — inline injection in the user turn triggers the agent's verify-via-tool reflex and defeats the purpose of ambient delivery.

---

## 4. Negative ambient marker

When retrieval returns no usable snippets (zero chunks, all low tier, below threshold, …), the response carries a structured marker instead of an empty payload. Hosts can surface this to the agent as a binary "memory checked, nothing relevant" signal — distinct from "memory wasn't checked at all" (where `marker_if_empty: null`). These two states lead to different agent behavior (fall back to `memory_search` vs treat as no data).

### 4.1 Reason enum (7 values)

| Reason | Meaning |
|---|---|
| `below_threshold` | Top-1 chunk below the low-tier confidence threshold. |
| `zero_chunks` | Retrieval returned no candidates at all. |
| `cold_start_grace` | Brand-new user inside the early-turns grace window — normally suppressed; emitted only when explicitly requested for telemetry. |
| `scope_empty` | Caller's scope contains no chunks. |
| `all_low_tier` | All retrieved chunks scored below the threshold post-fusion. |
| `degraded_retrieval` | Pipeline error (storage offline, embedding failed, …) — see `debug.error`. |
| `timeout_partial` | Latency budget exhausted before the pipeline completed. |

### 4.2 Suppress cases (response = `{snippets: [], marker_if_empty: null}`)

1. **Cold-start grace** — the first few turns of a brand-new user (zero chunks in scope). The agent shouldn't see "memory checked, nothing relevant" on a user's very first interactions.
2. **Explicit suppress** — request flag `suppress_negative_marker: true`. The host has a planned fallback; the marker would just be noise.
3. **Scope-mismatch noise** — hint queries pointed at a scope other than the caller's; fall-through is cleaner than a misleading "checked memory" framing.

**`all_low_tier` is NEVER suppressed**, regardless of cold-start / explicit-suppress / scope-mismatch state. The agent always needs to know "memory was checked and all hits were weak", because the cost of a false suppress on weak hits is high (the agent assumes nothing was checked, then fabricates).

---

## 5. Server-side `debug` field

`debug` is included in every successful response (and most degraded ones). Its purpose is operational telemetry and eval tooling:

| Field | Type | Notes |
|---|---|---|
| `trace_ids` | array<string> | Chunk IDs that contributed to top-N snippets. |
| `signal_breakdown_per_snippet` | array<object> | Per-channel raw scores + ranks. |
| `latency_ms.retrieval` | u32 | BM25 + vector + fuse stage. |
| `latency_ms.synthesis` | u32 | Fusion + scoring + synthesis + truncation stage. |
| `latency_ms.total` | u32 | End-to-end including auth + cache lookup. |
| `error` | string\|absent | Set when `marker_if_empty.reason ∈ {degraded_retrieval, timeout_partial}`. |

Agent-host renderers: **STRIP THIS FIELD** before constructing the agent-visible context.

---

## 6. Auth & scope

`/v1/ambient` is on the protected route stack — same auth requirements as `/v1/search` and `/v1/store`. The caller MUST present a valid bearer token (unless the server runs in local passthrough mode with no key configured).

The server validates `payload.scope`:

- **Admin** — passes for any scope.
- **Member** — the caller's stream memberships MUST contain the trailing `:<id>` portion of `payload.scope`.
- **Self-reference** — `payload.scope` equal to the caller's primary stream is always permitted.

Mismatches return `403 Forbidden` with a non-leaking message.

---

## 7. Cache & invalidation

In-memory LRU per server instance:

- **Key:** BLAKE3 hash over `(user_id, scope, recent_turns, hint)` — length-prefix-encoded for collision resistance.
- **TTL:** 60 s default, override via `LOOMEM_AMBIENT_CACHE_TTL_SECS`.
- **Capacity:** 10 000 entries, LRU eviction when full.

The `suppress_negative_marker` and `refresh` request flags are **not** part of the cache key (toggling them shouldn't recompute retrieval).

Invalidation is **TTL-only**; there is no invalidate-on-write event bus. `refresh: true` on the request bypasses the cache for that call AND skips seeding the cache from the response — useful for explicit "give me fresh ambient" semantics.

---

## 8. Latency budget

| Percentile | Target | Rationale |
|---|---|---|
| p50 | ≤ 100 ms | Deterministic templates trivially achieve; storage + embedding dominate. |
| p95 | ≤ 200 ms | Cold cache + larger candidate pool. |

The hot path is wrapped in a 100 ms timeout. On expiry, the response carries `marker_if_empty.reason: "timeout_partial"` (HTTP 200 — graceful fallback). Hosts SHOULD treat this like `degraded_retrieval` — fall back to `memory_search` rather than blocking on a partial response.

---

## 9. Token budget

Per-injection hard cap: **1500 tokens** total. Per-snippet cap: **200 tokens**. Marker floor: **50 tokens** — the marker is never dropped for budget reasons; the compression strategy drops low-tier positive snippets first.

| Tier | Drop priority (lower = drops first) |
|---|---|
| `low` | 0 |
| `medium` | 1 |
| `high` | 2 |
| `conflict` | 3 |

Token counting uses `tiktoken-rs` cl100k_base (OpenAI-compatible), initialized once per process.

---

## 10. Host integration checklist

An agent host integrating ambient memory:

1. **Calls** `/v1/ambient` once per agent turn.
2. **Reads** `snippets` + `marker_if_empty` ONLY from the response. Strips `debug`.
3. **Renders** snippets into the agent's system prompt with explicit per-tier behavior prescriptions (§3.2).
4. **Does not** reformat snippet `text`. The plain-fact constraint is server-enforced; client-side rewriting risks reintroducing metawords.
5. **Does not** inject the `debug` field anywhere agent-visible.

Whether ambient delivery improves your workload is empirical — internal evaluation showed strong gains on preference-style recall but no across-the-board improvement. Measure with your own traffic before making it the default (see `docs/architecture/memory-routing.md` §5).

---

**Implementation:** `loomem-server/src/handlers/ambient.rs` (handler) + `loomem-core/src/ambient/{cache,marker,retrieval,synthesis,types}.rs` (modules).
