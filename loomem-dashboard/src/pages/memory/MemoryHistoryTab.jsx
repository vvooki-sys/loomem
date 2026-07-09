import { useEffect, useState } from "react";
import { RefreshCw } from "lucide-react";
import { getMemoryChain } from "../../lib/api";
import { wordDiff } from "../../lib/wordDiff";
import ErrorState from "../../components/shared/ErrorState";

// Cycle/163 S4 — version history inside the detail panel (no full-page swap).
// Lists versions from /v1/memory-chain/{id}: date + change source + content.
// Error → ErrorState (never an eternal spinner); empty → explicit copy.
//
// Cycle/173 S3 — a word-level diff (green = added, struck-out red = removed)
// renders under every version after the first, comparing it to the previous
// version in the chain. Supersede-on-edit (/173 S1) means manual dashboard
// edits now appear in this chain too, not just machine changes.

function fmtTs(sec) {
  if (!sec) return "";
  return new Date(sec * 1000).toISOString().slice(0, 16).replace("T", " ");
}

// Render a word-level diff of prev → current content. Additions are tinted with
// the success token; deletions are struck through with the danger token.
function DiffView({ prev, current }) {
  const segments = wordDiff(prev, current);
  return (
    <div className="mt-2 pt-2 border-t border-[var(--border)] text-xs leading-relaxed whitespace-pre-wrap">
      <div className="text-[9px] uppercase tracking-wide text-[var(--text-subtle)] mb-1">
        Changes from previous
      </div>
      {segments.map((seg, i) =>
        seg.type === "eq" ? (
          <span key={i} className="text-[var(--text-muted)]">
            {seg.text}
          </span>
        ) : seg.type === "add" ? (
          <span
            key={i}
            className="rounded-sm bg-[var(--badge-active-bg)] text-[var(--success)]"
          >
            {seg.text}
          </span>
        ) : (
          <span
            key={i}
            className="rounded-sm bg-[var(--danger-bg)] text-[var(--danger)] line-through"
          >
            {seg.text}
          </span>
        ),
      )}
    </div>
  );
}

export default function MemoryHistoryTab({ chunkId }) {
  const [chain, setChain] = useState(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState(null);
  const [reloadKey, setReloadKey] = useState(0);

  useEffect(() => {
    if (!chunkId) return undefined;
    let cancelled = false;
    setLoading(true);
    setError(null);
    getMemoryChain(chunkId)
      .then((data) => {
        if (!cancelled) {
          setChain(data);
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
  }, [chunkId, reloadKey]);

  if (loading) {
    return (
      <div className="p-6 text-center text-xs text-[var(--text-muted)] flex items-center justify-center gap-2">
        <RefreshCw size={13} className="animate-spin" /> Loading version history…
      </div>
    );
  }

  if (error) {
    return (
      <div className="p-3">
        <ErrorState error={error} onRetry={() => setReloadKey((k) => k + 1)} compact />
      </div>
    );
  }

  const versions = chain?.chain || [];
  if (versions.length === 0) {
    return <div className="p-6 text-center text-xs text-[var(--text-subtle)]">No recorded versions</div>;
  }

  return (
    <div className="p-3 space-y-2">
      {versions.map((v, idx) => (
        <div
          key={v.id}
          className={`bg-[var(--bg)] border rounded-lg p-3 ${v.is_latest ? "border-[var(--accent)]/40" : "border-[var(--border)]"}`}
        >
          <div className="flex items-center gap-2 mb-1.5">
            <span className="text-[10px] font-mono font-semibold text-[var(--accent)]">v{v.version}</span>
            {v.is_latest && (
              <span className="text-[9px] px-1.5 py-0.5 rounded-full bg-[var(--row-selected)] text-[var(--accent)]">latest</span>
            )}
            <span className="text-[10px] text-[var(--text-subtle)] ml-auto">{fmtTs(v.timestamp)}</span>
          </div>
          <div className="text-xs text-[var(--text)] leading-relaxed whitespace-pre-wrap">{v.content}</div>
          {idx > 0 && <DiffView prev={versions[idx - 1].content} current={v.content} />}
          {v.supersedes_id && (
            <div className="text-[10px] text-[var(--text-subtle)] mt-1">supersedes: {v.supersedes_id.slice(0, 8)}…</div>
          )}
        </div>
      ))}
    </div>
  );
}
