import { AlertTriangle, RefreshCw } from "lucide-react";
import { errorCopy } from "../../lib/errorCopy";

// Inline error panel (cycle/152 §3.2 / §3.4). Sized to fill its parent panel —
// never a toast, never full-page. A 403 shows a permission message with no
// Retry (retrying won't help); every other error (5xx / network / parse)
// shows the cause plus a Retry button that calls onRetry to refetch.
//
// Props:
//   error   — the caught error (ApiError or any Error). Reads .status/.code.
//   onRetry — refetch callback; Retry hidden when absent or on 403.
//   compact — tighter padding for small chart-sized panels.
export default function ErrorState({ error, onRetry, compact = false }) {
  const status = typeof error?.status === "number" ? error.status : error?.code;
  const isForbidden = status === 403;
  const heading = isForbidden
    ? "You don't have permission to view this"
    : "Couldn't load this panel";

  return (
    <div
      className={`h-full w-full flex items-center justify-center ${compact ? "p-4" : "p-6"}`}
      role="alert"
    >
      <div className="text-center max-w-sm">
        <AlertTriangle size={compact ? 20 : 26} className="text-[var(--danger)] mx-auto mb-2" />
        <p className="text-[var(--text)] text-sm font-medium mb-1">{heading}</p>
        {!isForbidden && (
          <p className="text-[var(--text-muted)] text-xs mb-3 break-words">
            {errorCopy(error)}
          </p>
        )}
        {!isForbidden && onRetry && (
          <button
            type="button"
            onClick={onRetry}
            className="inline-flex items-center gap-1.5 px-3 py-1.5 rounded-full border border-[var(--border-strong)] text-xs text-[var(--text)] hover:bg-[var(--row-hover)] transition-all hover:-translate-y-[1px]"
          >
            <RefreshCw size={12} /> Retry
          </button>
        )}
      </div>
    </div>
  );
}
