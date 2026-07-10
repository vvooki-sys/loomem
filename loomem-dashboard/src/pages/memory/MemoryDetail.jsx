import { useEffect, useState } from "react";
import { Check, Copy, Pencil, Trash2, X } from "lucide-react";
import { MEMORY_LAYERS } from "../../lib/constants";
import MemoryHistoryTab from "./MemoryHistoryTab";
import MemoryEntitiesTab from "./MemoryEntitiesTab";

// Cycle/163 S1/S3 — the detail panel. One honest edit UX (inline textarea,
// pessimistic: the server is the single source of truth, no local override
// cache) and one delete UX (inline confirm step), replacing the old browser
// dialogs. Tabs: Content / History (S4) / Entities (S5).

function fmtCreatedAt(sec) {
  if (!sec || typeof sec !== "number") return null;
  const d = new Date(sec * 1000);
  const pad = (n) => String(n).padStart(2, "0");
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}`;
}

function MetaRow({ label, children }) {
  return (
    <div className="flex items-baseline gap-2 text-[11px]">
      <span className="text-[var(--text-subtle)] uppercase tracking-wider w-24 shrink-0">{label}</span>
      <span className="text-[var(--text)] min-w-0 break-words">{children}</span>
    </div>
  );
}

export default function MemoryDetail({ memory, readOnly, onEdit, onDelete, onClose }) {
  const [tab, setTab] = useState("content");
  const [mode, setMode] = useState("view");
  const [draft, setDraft] = useState(memory.content);
  const [saving, setSaving] = useState(false);
  const [confirmDelete, setConfirmDelete] = useState(false);
  const [deleting, setDeleting] = useState(false);
  const [copied, setCopied] = useState(false);

  // Reset transient UI when the selected memory changes.
  useEffect(() => {
    setTab("content");
    setMode("view");
    setDraft(memory.content);
    setConfirmDelete(false);
  }, [memory.id, memory.content]);

  const save = async () => {
    if (saving) return;
    setSaving(true);
    try {
      await onEdit(memory.id, { content: draft });
      setMode("view"); // success — parent refreshed the list; content flows via props
    } catch {
      // Parent toasts the cause; keep the editor open with the typed text (S3).
    } finally {
      setSaving(false);
    }
  };

  const remove = async () => {
    if (deleting) return;
    setDeleting(true);
    try {
      await onDelete(memory.id); // parent clears selection + refreshes on success
    } catch {
      setConfirmDelete(false); // parent toasts; drop back to the actions row
    } finally {
      setDeleting(false);
    }
  };

  const copyId = () => {
    navigator.clipboard?.writeText(memory.id).then(
      () => {
        setCopied(true);
        window.setTimeout(() => setCopied(false), 1200);
      },
      () => {},
    );
  };

  const m = memory;
  const created = fmtCreatedAt(m.created_at);

  return (
    <div className="flex flex-col h-full min-w-0 bg-[var(--panel)]">
      <header className="flex items-center justify-between gap-2 px-4 py-3 border-b border-[var(--border)] shrink-0">
        <div className="flex items-center gap-2 min-w-0">
          <span
            className="px-1.5 py-0.5 rounded text-[10px] font-mono font-semibold shrink-0"
            style={{
              backgroundColor: (MEMORY_LAYERS[m.layer]?.color || "#6b7280") + "20",
              color: MEMORY_LAYERS[m.layer]?.color || "#6b7280",
            }}
          >
            {m.layer}
          </span>
          <button
            type="button"
            onClick={copyId}
            title="Copy ID"
            className="flex items-center gap-1 text-[11px] font-mono text-[var(--text-subtle)] hover:text-[var(--text)] truncate"
          >
            <span className="truncate">{m.id}</span>
            {copied ? <Check size={11} className="text-[var(--success)] shrink-0" /> : <Copy size={11} className="shrink-0" />}
          </button>
        </div>
        {onClose && (
          <button type="button" onClick={onClose} className="text-[var(--text-muted)] hover:text-[var(--text)] shrink-0" aria-label="Close">
            <X size={16} />
          </button>
        )}
      </header>

      <nav className="flex border-b border-[var(--border)] text-[12px] shrink-0">
        {[
          ["content", "Content"],
          ["history", "History"],
          ["entities", "Entities"],
        ].map(([id, label]) => (
          <button
            key={id}
            type="button"
            onClick={() => setTab(id)}
            className={`px-4 py-2 border-b-2 ${
              tab === id
                ? "border-[var(--accent)] text-[var(--accent)] font-medium"
                : "border-transparent text-[var(--text-muted)] hover:text-[var(--text)]"
            }`}
          >
            {label}
          </button>
        ))}
      </nav>

      <div className="flex-1 overflow-y-auto">
        {tab === "content" && (
          <div className="p-4 space-y-4">
            {mode === "edit" ? (
              <div className="space-y-2">
                <textarea
                  value={draft}
                  onChange={(e) => setDraft(e.target.value)}
                  rows={6}
                  autoFocus
                  aria-label="Edit memory content"
                  className="w-full bg-[var(--bg)] border border-[var(--accent)] rounded-lg text-[13px] text-[var(--text)] p-3 leading-relaxed resize-y focus:outline-none"
                />
                <div className="flex items-center gap-2">
                  <button
                    type="button"
                    onClick={save}
                    disabled={saving}
                    className="flex items-center gap-1 px-3 py-1.5 rounded-full text-xs bg-[var(--accent)] text-white hover:bg-[var(--accent-hover)] disabled:opacity-50"
                  >
                    <Check size={12} /> {saving ? "Saving…" : "Save"}
                  </button>
                  <button
                    type="button"
                    onClick={() => {
                      setDraft(m.content);
                      setMode("view");
                    }}
                    disabled={saving}
                    className="flex items-center gap-1 px-3 py-1.5 rounded-full text-xs bg-[var(--row-hover)] text-[var(--text-muted)] hover:text-[var(--text)] disabled:opacity-50"
                  >
                    <X size={12} /> Cancel
                  </button>
                </div>
              </div>
            ) : (
              <p className="text-[14px] text-[var(--text)] leading-relaxed whitespace-pre-wrap">{m.content}</p>
            )}

            <div className="space-y-1.5 pt-3 border-t border-[var(--border)]">
              {typeof m.confidence === "number" && (
                <MetaRow label="Confidence">{(m.confidence * 100).toFixed(0)}%</MetaRow>
              )}
              {typeof m.decay === "number" && (
                <MetaRow label="Freshness">
                  {m.decay < 0.05 ? "fresh" : m.decay < 0.15 ? "fading" : "decaying"}
                </MetaRow>
              )}
              {m.source_agent && <MetaRow label="Source">{m.source_agent}</MetaRow>}
              {m.event_date && <MetaRow label="Event date">{m.event_date}</MetaRow>}
              {created && <MetaRow label="Created">{created}</MetaRow>}
              {m.category && <MetaRow label="Category">{m.category}</MetaRow>}
              {typeof m.score === "number" && <MetaRow label="Score">{(m.score * 100).toFixed(0)}%</MetaRow>}
            </div>

            {!readOnly && mode === "view" && (
              <div className="flex items-center gap-2 pt-3 border-t border-[var(--border)]">
                <button
                  type="button"
                  onClick={() => setMode("edit")}
                  className="flex items-center gap-1 px-3 py-1.5 rounded-full text-xs border border-[var(--border-strong)] text-[var(--text)] hover:bg-[var(--row-hover)]"
                >
                  <Pencil size={12} /> Edit
                </button>
                {confirmDelete ? (
                  <div className="flex items-center gap-2">
                    <button
                      type="button"
                      onClick={remove}
                      disabled={deleting}
                      className="flex items-center gap-1 px-3 py-1.5 rounded-full text-xs bg-[var(--danger)] text-white hover:opacity-90 disabled:opacity-50"
                    >
                      <Trash2 size={12} /> {deleting ? "Deleting…" : "Confirm delete"}
                    </button>
                    <button
                      type="button"
                      onClick={() => setConfirmDelete(false)}
                      className="px-3 py-1.5 rounded-full text-xs bg-[var(--row-hover)] text-[var(--text-muted)] hover:text-[var(--text)]"
                    >
                      Cancel
                    </button>
                  </div>
                ) : (
                  <button
                    type="button"
                    onClick={() => setConfirmDelete(true)}
                    className="flex items-center gap-1 px-3 py-1.5 rounded-full text-xs border border-[var(--danger)]/40 text-[var(--danger)] hover:bg-[var(--danger-bg)]"
                  >
                    <Trash2 size={12} /> Delete
                  </button>
                )}
              </div>
            )}
          </div>
        )}

        {tab === "history" && <MemoryHistoryTab chunkId={m.id} />}
        {tab === "entities" && <MemoryEntitiesTab entityIds={m.entity_ids} />}
      </div>
    </div>
  );
}
