import { Brain, RefreshCw } from "lucide-react";
import IdentityBanner from "./IdentityBanner";
import MemoryToolbar from "./MemoryToolbar";
import MemoryRow from "./MemoryRow";
import ErrorState from "../../components/shared/ErrorState";
import EmptyState from "../../components/shared/EmptyState";

// Cycle/163 S1 — master column: identity banner + toolbar over a dense,
// server-paginated row list. The list never disappears on selection; states
// (loading / error / empty) render inside the scroll area, below the toolbar,
// so search controls stay usable.

export default function MemoryList({
  streamId,
  items,
  total,
  loading,
  error,
  onRetry,
  query,
  onQuery,
  mode,
  onMode,
  layer,
  onLayer,
  agents,
  sourceAgent,
  onSourceAgent,
  selectedId,
  onSelect,
  page,
  perPage,
  onPage,
  onPerPage,
}) {
  const totalPages = Math.max(1, Math.ceil(total / perPage));
  const hasFilter = Boolean(query) || layer !== "all" || Boolean(sourceAgent);

  return (
    <div className="flex flex-col min-w-0 h-full bg-[var(--panel)] border-r border-[var(--border)]">
      <IdentityBanner streamId={streamId} total={total} />
      <MemoryToolbar
        query={query}
        onQuery={onQuery}
        mode={mode}
        onMode={onMode}
        layer={layer}
        onLayer={onLayer}
        agents={agents}
        sourceAgent={sourceAgent}
        onSourceAgent={onSourceAgent}
      />

      <div className="flex-1 overflow-y-auto">
        {loading && !items.length ? (
          <div className="p-8 text-center text-[var(--text-subtle)] text-xs flex items-center justify-center gap-2">
            <RefreshCw size={14} className="animate-spin" /> Loading memories…
          </div>
        ) : error ? (
          <ErrorState error={error} onRetry={onRetry} compact />
        ) : items.length === 0 ? (
          <EmptyState
            icon={<Brain size={26} />}
            title={hasFilter ? "No matching memories" : "No memories yet"}
            hint={
              hasFilter
                ? "Try a different query, layer, or search mode."
                : "Memories appear here once you ingest them via MCP or the API."
            }
          />
        ) : (
          items.map((m) => (
            <MemoryRow key={m.id} memory={m} selected={selectedId === m.id} onSelect={onSelect} />
          ))
        )}
      </div>

      {mode === "substring" && total > perPage && (
        <div className="px-4 py-2.5 border-t border-[var(--border)] bg-[var(--bg)] flex items-center justify-between gap-3 text-xs">
          <div className="flex items-center gap-2 text-[var(--text-muted)]">
            <span>Per page:</span>
            {[25, 50, 100].map((n) => (
              <button
                key={n}
                type="button"
                onClick={() => onPerPage(n)}
                className={`px-2 py-0.5 rounded ${perPage === n ? "bg-[var(--row-hover)] text-[var(--text)]" : "hover:text-[var(--text)]"}`}
              >
                {n}
              </button>
            ))}
          </div>
          <div className="flex items-center gap-3 text-[var(--text-muted)]">
            <button
              type="button"
              onClick={() => onPage(Math.max(1, page - 1))}
              disabled={page <= 1}
              className="px-2.5 py-1 rounded border border-[var(--border)] disabled:opacity-40 disabled:cursor-not-allowed hover:text-[var(--text)]"
            >
              « Prev
            </button>
            <span className="font-mono">
              Page {page} of {totalPages}
            </span>
            <button
              type="button"
              onClick={() => onPage(Math.min(totalPages, page + 1))}
              disabled={page >= totalPages}
              className="px-2.5 py-1 rounded border border-[var(--border)] disabled:opacity-40 disabled:cursor-not-allowed hover:text-[var(--text)]"
            >
              Next »
            </button>
          </div>
        </div>
      )}
    </div>
  );
}
