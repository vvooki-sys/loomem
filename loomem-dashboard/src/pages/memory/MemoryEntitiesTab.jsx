import { useEffect, useState } from "react";
import { ChevronLeft, RefreshCw } from "lucide-react";
import { getMemory } from "../../lib/api";
import { MEMORY_LAYERS } from "../../lib/constants";
import ErrorState from "../../components/shared/ErrorState";

// Related entities live INSIDE the detail panel. Clicking an entity fetches
// its memories server-side (entity_id=) and shows them here with a breadcrumb
// back to the chunk. It never changes the route.

export default function MemoryEntitiesTab({ entityIds }) {
  const [entityId, setEntityId] = useState(null);
  const [memories, setMemories] = useState([]);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState(null);

  useEffect(() => {
    if (!entityId) return undefined;
    let cancelled = false;
    setLoading(true);
    setError(null);
    getMemory({ entity_id: entityId, per_page: 50 })
      .then((data) => {
        if (!cancelled) {
          setMemories(data?.items || []);
          setLoading(false);
        }
      })
      .catch((err) => {
        if (!cancelled) {
          setError(err);
          setLoading(false);
        }
      });
    return () => {
      cancelled = true;
    };
  }, [entityId]);

  if (!entityIds || entityIds.length === 0) {
    return <div className="p-6 text-center text-xs text-[var(--text-subtle)]">No entities linked to this memory</div>;
  }

  if (!entityId) {
    return (
      <div className="p-3 flex flex-wrap gap-1.5">
        {entityIds.map((eid) => (
          <button
            key={eid}
            type="button"
            onClick={() => setEntityId(eid)}
            className="text-xs px-2.5 py-1 rounded-full border border-[var(--border)] text-[var(--text-muted)] hover:border-[var(--border-strong)] hover:text-[var(--text)] font-mono transition-colors"
          >
            {eid.slice(0, 8)}…
          </button>
        ))}
      </div>
    );
  }

  return (
    <div className="p-3 space-y-2">
      <button
        type="button"
        onClick={() => setEntityId(null)}
        className="flex items-center gap-1 text-xs text-[var(--text-muted)] hover:text-[var(--text)]"
      >
        <ChevronLeft size={13} /> Back to memory
      </button>
      <div className="text-[10px] font-mono text-[var(--text-subtle)]">entity {entityId.slice(0, 8)}…</div>
      {loading ? (
        <div className="p-4 text-center text-xs text-[var(--text-muted)] flex items-center justify-center gap-2">
          <RefreshCw size={13} className="animate-spin" /> Loading…
        </div>
      ) : error ? (
        <ErrorState error={error} compact />
      ) : memories.length === 0 ? (
        <div className="p-4 text-center text-xs text-[var(--text-subtle)]">No memories for this entity</div>
      ) : (
        memories.map((m) => (
          <div key={m.id} className="bg-[var(--bg)] border border-[var(--border)] rounded-lg p-2.5">
            <div className="flex items-center gap-2 mb-1">
              <span
                className="px-1.5 py-0.5 rounded text-[10px] font-mono font-semibold"
                style={{
                  backgroundColor: (MEMORY_LAYERS[m.layer]?.color || "#8E8474") + "20",
                  color: MEMORY_LAYERS[m.layer]?.color || "#8E8474",
                }}
              >
                {m.layer}
              </span>
              {m.event_date && <span className="text-[10px] text-[var(--text-subtle)] font-mono">{m.event_date}</span>}
            </div>
            <p className="text-xs text-[var(--text)] leading-snug">{m.content}</p>
          </div>
        ))
      )}
    </div>
  );
}
