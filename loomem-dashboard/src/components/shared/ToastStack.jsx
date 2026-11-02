export default function ToastStack({ toasts, onDismiss }) {
  if (!toasts.length) return null;
  return (
    <div className="fixed bottom-6 right-6 z-50 flex flex-col gap-2 max-w-sm">
      {toasts.map((t) => (
        <div
          key={t.id}
          role="status"
          className={`rounded-md border px-4 py-2.5 text-[13px] shadow-xs bg-(--panel) animate-fade-in flex items-start gap-3 ${
            t.tone === "error"
              ? "border-[#fecaca] text-(--danger)"
              : t.tone === "warn"
                ? "border-[#fcd34d] text-(--warn)"
                : "border-(--border) text-(--text)"
          }`}
        >
          <span className="flex-1">{t.message}</span>
          <button
            type="button"
            onClick={() => onDismiss(t.id)}
            className="text-(--text-subtle) hover:text-(--text)"
          >
            ✕
          </button>
        </div>
      ))}
    </div>
  );
}
