import { useCallback, useEffect, useRef, useState } from "react";
import { getMemory, searchMemories } from "../../lib/api";

// Server-side memory list. Two modes over ONE row shape:
//   • substring (default): GET /api/dashboard/memory?q=&layer=&page=
//     — the server filters the whole stream, not the current page.
//   • semantic: POST /v1/search — hybrid BM25+vector, results carry a score.
//
// No auto-poll: the list refetches on param change and on explicit refresh()
// after a mutation (the Memory list is browse-then-edit; a 30s background
// poll fights the editor). A per-call token guards against a slow response
// overwriting a newer one.

// Map a /v1/search result onto the /api/dashboard/memory row shape so the list
// and detail render identically regardless of mode. Fields the search endpoint
// does not carry (category, entity_ids, event date beyond timestamp) are left
// null — the detail panel degrades gracefully.
function mapSearchResults(raw) {
  const results = raw?.results || [];
  const items = results.map((r) => ({
    id: r.id,
    content: r.content,
    layer: r.metadata?.level === 1 ? "L1" : "L0",
    confidence: Math.min(r.score ?? 0, 1),
    decay: r.metadata?.time_decay != null ? Math.max(0, 1 - r.metadata.time_decay) : 0,
    score: r.score,
    created_at: r.metadata?.timestamp ?? null,
    event_date: r.metadata?.timestamp
      ? new Date(r.metadata.timestamp * 1000).toISOString().slice(0, 10)
      : null,
    source_agent: r.metadata?.source_agent ?? null,
    category: null,
    entity_ids: [],
    version: 1,
  }));
  return { items, total: items.length, mode: "semantic" };
}

export default function useMemoryList({ query, mode, layer, sourceAgent, page, perPage }) {
  const [data, setData] = useState(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState(null);
  const tokenRef = useRef(0);

  const q = (query || "").trim();

  const fetchList = useCallback(async () => {
    const myToken = ++tokenRef.current;
    setLoading(true);
    setError(null);
    try {
      let result;
      if (mode === "semantic" && q) {
        // /v1/search has no layer param, so semantic mode does not layer-filter
        // at all: filtering the ranked top-k client-side would silently drop
        // matching rows that rank below the window (starvation). The layer
        // control is hidden + reset to "all" in semantic mode instead.
        result = mapSearchResults(await searchMemories(q, { top_k: perPage }));
      } else {
        const params = { page, per_page: perPage };
        if (q) params.q = q;
        if (layer !== "all") params.layer = layer;
        if (sourceAgent) params.source_agent = sourceAgent;
        result = { ...(await getMemory(params)), mode: "substring" };
      }
      if (myToken === tokenRef.current) setData(result);
    } catch (err) {
      if (myToken === tokenRef.current) setError(err);
    } finally {
      if (myToken === tokenRef.current) setLoading(false);
    }
  }, [q, mode, layer, sourceAgent, page, perPage]);

  useEffect(() => {
    fetchList();
  }, [fetchList]);

  // Merge an authoritative server response (or the just-saved fields) into a
  // single row without a full refetch, so the detail reflects the edit
  // immediately and stays correct even if the follow-up refresh() fails.
  const patchItem = useCallback((id, patch) => {
    setData((prev) => {
      if (!prev?.items) return prev;
      return { ...prev, items: prev.items.map((m) => (m.id === id ? { ...m, ...patch } : m)) };
    });
  }, []);

  return { data, loading, error, refresh: fetchList, patchItem };
}
