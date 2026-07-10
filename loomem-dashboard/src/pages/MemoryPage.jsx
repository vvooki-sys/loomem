// Memory as a master-detail page, collapsed to single-user: one instance,
// one stream — no scope toggles, no project selectors. Server-side search
// (substring / semantic), one honest inline edit/delete UX, version history
// + related entities in the detail panel.

import { useCallback, useEffect, useMemo, useState } from "react";
import useToasts from "../hooks/useToasts";
import useMediaQuery from "../hooks/useMediaQuery";
import ToastStack from "../components/shared/ToastStack";
import MemoryList from "./memory/MemoryList";
import MemoryDetail from "./memory/MemoryDetail";
import useMemoryList from "./memory/useMemoryList";
import { updateMemory, deleteMemory } from "../lib/api";
import { errorCopy } from "../lib/errorCopy";

export default function MemoryPage({ userCtx }) {
  const { toasts, push, dismiss } = useToasts();
  const isWide = useMediaQuery("(min-width: 1100px)");

  // ── Search / filter / pagination / selection state ──
  const [query, setQuery] = useState("");
  const [debouncedQuery, setDebouncedQuery] = useState("");
  const [mode, setMode] = useState("substring");
  const [layer, setLayer] = useState("all");
  const [sourceAgent, setSourceAgent] = useState("");
  const [page, setPage] = useState(1);
  const [perPage, setPerPage] = useState(50);
  const [selectedId, setSelectedId] = useState(null);

  // Debounce the search box (300ms) before it hits the server.
  useEffect(() => {
    const t = window.setTimeout(() => setDebouncedQuery(query), 300);
    return () => window.clearTimeout(t);
  }, [query]);

  // Reset to page 1 whenever the result set can change out from under us.
  useEffect(() => {
    setPage(1);
  }, [debouncedQuery, mode, layer, sourceAgent, perPage]);

  const { data, loading, error, refresh, patchItem } = useMemoryList({
    query: debouncedQuery,
    mode,
    layer,
    sourceAgent,
    page,
    perPage,
  });

  const items = useMemo(() => data?.items || [], [data]);
  const total = data?.total ?? 0;

  // Accumulate the distinct source_agent values seen across list responses so
  // the toolbar dropdown does not collapse to one option once a filter is
  // active (server-filtered responses only carry the selected agent).
  const [agentOptions, setAgentOptions] = useState([]);
  useEffect(() => {
    const found = items.map((i) => i.source_agent).filter(Boolean);
    if (found.length === 0) return;
    setAgentOptions((prev) => {
      const merged = new Set(prev);
      found.forEach((a) => merged.add(a));
      return merged.size === prev.length ? prev : [...merged].sort();
    });
  }, [items]);

  // ── Mutations: pessimistic, server is the source of truth (no overrides) ──
  const handleEdit = useCallback(
    async (id, updates) => {
      try {
        const updated = await updateMemory(id, updates);
        const newId = updated?.id && updated.id !== id ? updated.id : null;
        if (newId) {
          // A content edit may supersede the old chunk with a NEW id
          // (bitemporal version). Refresh so the new version is in the list
          // first, THEN follow the selection to it, so the detail never
          // points at an id that isn't in `items`.
          await refresh();
          setSelectedId(newId);
        } else {
          // Same-id edit: optimistic patch for instant feedback (reflecting
          // the server body when present), then reconcile.
          patchItem(id, updated && typeof updated.content === "string" ? updated : updates);
          refresh();
        }
        push({ tone: "success", message: "Memory updated" });
      } catch (err) {
        push({ tone: "error", message: errorCopy(err) });
        throw err;
      }
    },
    [push, refresh, patchItem],
  );

  const handleDelete = useCallback(
    async (id) => {
      try {
        await deleteMemory(id);
        push({ tone: "success", message: "Memory deleted" });
        setSelectedId(null);
        refresh();
      } catch (err) {
        push({ tone: "error", message: errorCopy(err) });
        throw err;
      }
    },
    [push, refresh],
  );

  const selectedMemory = selectedId ? items.find((m) => m.id === selectedId) ?? null : null;

  const list = (
    <MemoryList
      streamId={userCtx?.stream_id}
      items={items}
      total={total}
      loading={loading}
      error={error}
      onRetry={refresh}
      query={query}
      onQuery={setQuery}
      mode={mode}
      onMode={(m) => {
        setMode(m);
        // Layer filtering is substring-only; drop any stale layer when entering
        // semantic so the (hidden) filter never lies about the shown results.
        if (m === "semantic") setLayer("all");
      }}
      layer={layer}
      onLayer={setLayer}
      agents={agentOptions}
      sourceAgent={sourceAgent}
      onSourceAgent={setSourceAgent}
      selectedId={selectedId}
      onSelect={setSelectedId}
      page={page}
      perPage={perPage}
      onPage={setPage}
      onPerPage={setPerPage}
    />
  );

  const detail = selectedMemory ? (
    <MemoryDetail
      memory={selectedMemory}
      readOnly={false}
      onEdit={handleEdit}
      onDelete={handleDelete}
      onClose={isWide ? null : () => setSelectedId(null)}
    />
  ) : (
    <EmptyDetail />
  );

  return (
    <div className="h-full w-full flex flex-col">
      {/* grid-rows-[minmax(0,1fr)] caps the implicit grid row at the container
          height. Without it grid items keep min-height:auto, grow to content
          height, and the inner overflow-y-auto containers never get a bounded
          height — so the list cannot scroll and pagination sits below the fold. */}
      <div className="flex-1 min-h-0">
        {isWide ? (
          <div className="h-full grid grid-rows-[minmax(0,1fr)] overflow-hidden" style={{ gridTemplateColumns: "minmax(0, 46%) minmax(0, 54%)" }}>
            {list}
            <aside className="min-w-0 overflow-hidden">{detail}</aside>
          </div>
        ) : (
          <div className="h-full relative overflow-hidden">
            {list}
            {selectedMemory && (
              <div className="fixed inset-0 z-40 flex justify-end" role="dialog" aria-modal="true">
                <button
                  type="button"
                  className="absolute inset-0 bg-black/40"
                  aria-label="Close"
                  onClick={() => setSelectedId(null)}
                />
                <aside className="relative w-full max-w-[520px] bg-[var(--panel)] border-l border-[var(--border)] shadow-2xl">
                  {detail}
                </aside>
              </div>
            )}
          </div>
        )}
      </div>

      <ToastStack toasts={toasts} onDismiss={dismiss} />
    </div>
  );
}

function EmptyDetail() {
  return (
    <div className="h-full flex items-center justify-center text-center p-10 bg-[var(--panel)]">
      <div>
        <div className="text-[14px] text-[var(--text-muted)] mb-1">Select a memory</div>
        <div className="text-[12px] text-[var(--text-subtle)]">Click a row to see its full content, history, and entities.</div>
      </div>
    </div>
  );
}
