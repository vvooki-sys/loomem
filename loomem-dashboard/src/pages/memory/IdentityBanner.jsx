import { Database } from "lucide-react";

// The context bar that answers "which stream, how much" before the list ever
// loads. Single-user: there is one instance and one active stream — show the
// stream id rather than a user identity.

export default function IdentityBanner({ streamId, total }) {
  return (
    <div className="flex items-center gap-2 px-4 py-2 text-[12px] text-[var(--text-muted)] bg-[var(--panel)] border-b border-[var(--border)]">
      <Database size={13} className="shrink-0 text-[var(--text-subtle)]" />
      <span className="font-mono text-[var(--text)] truncate">{streamId || "local"}</span>
      <span className="text-[var(--text-subtle)]">·</span>
      <span className="tabular-nums">{total.toLocaleString()} memories</span>
    </div>
  );
}
