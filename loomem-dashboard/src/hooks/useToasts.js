import { useCallback, useEffect, useRef, useState } from "react";

let seq = 0;

// Suppress a repeat of the same message within this window (cycle/163 L-7 —
// double fetch/subscription firing two identical "Failed to load scopes"
// toasts). Keyed by message text; a later identical message is allowed once
// the window elapses.
const DEDUPE_MS = 3000;

export default function useToasts() {
  const [toasts, setToasts] = useState([]);
  const recentRef = useRef(new Map());

  const push = useCallback((toast) => {
    const key = toast?.message;
    if (key != null) {
      const now = Date.now();
      const last = recentRef.current.get(key);
      if (last != null && now - last < DEDUPE_MS) return;
      recentRef.current.set(key, now);
    }
    seq += 1;
    const id = seq;
    setToasts((cur) => [...cur, { id, ...toast }]);
    window.setTimeout(() => {
      setToasts((cur) => cur.filter((t) => t.id !== id));
    }, toast.duration ?? 3500);
  }, []);

  const dismiss = useCallback((id) => {
    setToasts((cur) => cur.filter((t) => t.id !== id));
  }, []);

  useEffect(() => () => setToasts([]), []);

  return { toasts, push, dismiss };
}
