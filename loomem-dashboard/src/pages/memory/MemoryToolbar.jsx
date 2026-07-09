import { Search } from "lucide-react";

// Cycle/163 S6 — server-side search + filters. Substring is the D4 default
// ("Filter"); "Semantic" flips to POST /v1/search. Layer filter is server-side
// (q/layer ride into /api/dashboard/memory).
//
// Cycle/173 S3 — the source_agent filter is now server-side too (rides into
// /api/dashboard/memory alongside q/layer). Options accumulate from list
// responses; it applies to substring mode only (/v1/search filtering is out of
// scope for this cycle).

const LAYERS = ["all", "L0", "L1"];

export default function MemoryToolbar({
  query,
  onQuery,
  mode,
  onMode,
  layer,
  onLayer,
  agents = [],
  sourceAgent = "",
  onSourceAgent,
}) {
  return (
    <div className="px-4 py-3 border-b border-[var(--border)] space-y-2.5 bg-[var(--panel)]">
      <div className="flex items-center gap-2">
        <div className="relative flex-1">
          <Search size={14} className="absolute left-3 top-1/2 -translate-y-1/2 text-[var(--text-muted)]" />
          <input
            type="text"
            value={query}
            onChange={(e) => onQuery(e.target.value)}
            placeholder={mode === "semantic" ? "Search by meaning…" : "Filter memories…"}
            className="w-full bg-[var(--bg)] border border-[var(--border)] rounded-lg pl-9 pr-3 py-2 text-[13px] text-[var(--text)] placeholder-[var(--text-subtle)] focus:outline-none focus:border-[var(--accent)]"
          />
        </div>
        <div
          role="tablist"
          aria-label="Search mode"
          className="inline-flex items-center gap-0.5 rounded-lg border border-[var(--border)] bg-[var(--bg)] p-0.5 shrink-0"
        >
          {[
            ["substring", "Filter"],
            ["semantic", "Semantic"],
          ].map(([value, label]) => (
            <button
              key={value}
              type="button"
              role="tab"
              aria-selected={mode === value}
              onClick={() => onMode(value)}
              className={`px-2.5 py-1 rounded-md text-xs transition-colors ${
                mode === value
                  ? "bg-[var(--row-selected)] text-[var(--accent)] font-medium"
                  : "text-[var(--text-muted)] hover:text-[var(--text)]"
              }`}
            >
              {label}
            </button>
          ))}
        </div>
      </div>
      {/* Layer is a substring-mode filter only — /v1/search has no layer
          param, so semantic mode hides it rather than fake a client-side pass. */}
      {mode === "substring" && (
      <div className="flex items-center gap-2 flex-wrap">
        <span className="text-xs text-[var(--text-muted)]">Layer:</span>
        {LAYERS.map((l) => (
          <button
            key={l}
            type="button"
            onClick={() => onLayer(l)}
            className={`text-xs px-2.5 py-1 rounded-full border transition-colors ${
              layer === l
                ? "border-[var(--accent)] bg-[var(--row-selected)] text-[var(--accent)]"
                : "border-[var(--border)] text-[var(--text-muted)] hover:text-[var(--text)]"
            }`}
          >
            {l === "all" ? "All" : l}
          </button>
        ))}
        {agents.length > 0 && (
          <label className="flex items-center gap-1.5 text-xs text-[var(--text-muted)] ml-1">
            Agent:
            <select
              value={sourceAgent}
              onChange={(e) => onSourceAgent?.(e.target.value)}
              className="bg-[var(--bg)] border border-[var(--border)] rounded-md px-2 py-1 text-xs text-[var(--text)] focus:outline-none focus:border-[var(--accent)]"
            >
              <option value="">All</option>
              {agents.map((a) => (
                <option key={a} value={a}>
                  {a}
                </option>
              ))}
            </select>
          </label>
        )}
      </div>
      )}
    </div>
  );
}
