export default function ToastStack({ toasts, onDismiss }) {
  if (!toasts.length) return null;
  return (
    <div className="fixed bottom-6 right-6 z-50 flex flex-col gap-2 max-w-sm">
      {toasts.map((t) => (
        <div
          key={t.id}
          role="status"
          className={`rounded-md border px-4 py-2.5 text-[13px] shadow-sm bg-[var(--panel)] animate-fade-in flex items-start gap-3 ${
            t.tone === "error"
              ? "border-[#fecaca] text-[var(--danger)]"
              : t.tone === "warn"
                ? "border-[#fcd34d] text-[var(--warn)]"
                : "border-[var(--border)] text-[var(--text)]"
          }`}
        >
          <span className="flex-1">{t.message}</span>
          <button
            type="button"
            onClick={() => onDismiss(t.id)}
            className="text-[var(--text-subtle)] hover:text-[var(--text)]"
          >
            ✕
          </button>
        </div>
      ))}
    </div>
  );
}
