import { Inbox } from "lucide-react";

// Inline empty state (cycle/152 §3.1). Rendered only when a fetch SUCCEEDED
// but returned zero records — semantically distinct from an error. Never used
// as an error fallback: "empty" must never stand in for "failed".
//
// Props:
//   title  — headline, e.g. "No memories yet".
//   hint   — optional secondary line.
//   action — optional React node (e.g. a button) shown below the hint.
//   icon   — optional icon node; defaults to an inbox glyph.
//   compact — tighter padding for small panels.
export default function EmptyState({ title, hint, action, icon, compact = false }) {
  return (
    <div className={`h-full w-full flex items-center justify-center ${compact ? "p-4" : "p-6"}`}>
      <div className="text-center max-w-sm">
        <div className="text-[var(--text-subtle)] mx-auto mb-2 flex items-center justify-center">
          {icon ?? <Inbox size={compact ? 20 : 26} />}
        </div>
        <p className="text-[var(--text-muted)] text-sm font-medium mb-1">{title}</p>
        {hint && <p className="text-[var(--text-subtle)] text-xs">{hint}</p>}
        {action && <div className="mt-3">{action}</div>}
      </div>
    </div>
  );
}
