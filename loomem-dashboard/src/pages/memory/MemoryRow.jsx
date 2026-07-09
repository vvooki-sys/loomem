import { MEMORY_LAYERS } from "../../lib/constants";

// Cycle/163 S1 — dense master-list row (not the old two-column cards). Click
// selects; the detail panel opens beside it and the list never disappears.

function confidenceColor(c) {
  if (c > 0.9) return "var(--success)";
  if (c > 0.7) return "var(--warn)";
  return "var(--danger)";
}

export default function MemoryRow({ memory, selected, onSelect }) {
  const m = memory;
  return (
    <button
      type="button"
      onClick={() => onSelect(m.id)}
      aria-pressed={selected}
      className={`w-full text-left px-4 py-2.5 border-b border-[var(--border)] transition-colors ${
        selected ? "bg-[var(--row-selected)]" : "hover:bg-[var(--row-hover)]"
      }`}
    >
      <div className="flex items-center gap-2 mb-1">
        <span
          className="px-1.5 py-0.5 rounded text-[10px] font-mono font-semibold shrink-0"
          style={{
            backgroundColor: (MEMORY_LAYERS[m.layer]?.color || "#6b7280") + "20",
            color: MEMORY_LAYERS[m.layer]?.color || "#6b7280",
          }}
        >
          {m.layer}
        </span>
        {m.source_agent && (
          <span className="text-[10px] text-[var(--text-subtle)] font-mono truncate">{m.source_agent}</span>
        )}
        {typeof m.score === "number" && (
          <span className="text-[10px] text-[var(--accent)] font-mono shrink-0">
            {(m.score * 100).toFixed(0)}%
          </span>
        )}
        <span className="ml-auto flex items-center gap-2 shrink-0">
          {typeof m.confidence === "number" && (
            <span
              className="w-1.5 h-1.5 rounded-full"
              style={{ backgroundColor: confidenceColor(m.confidence) }}
              title={`confidence ${(m.confidence * 100).toFixed(0)}%`}
            />
          )}
          {m.event_date && (
            <span className="text-[10px] text-[var(--text-subtle)] font-mono">{m.event_date}</span>
          )}
        </span>
      </div>
      <p className="text-[13px] text-[var(--text)] leading-snug line-clamp-2">{m.content}</p>
    </button>
  );
}
