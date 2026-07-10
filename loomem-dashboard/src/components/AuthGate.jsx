import { useState, useEffect } from "react";
import LoginHeroRing from "./LoginHeroRing";

// Single-user token unlock. "Login" here is unlocking THIS instance with its
// one API token — not user authentication (Loomem has no accounts). The token
// is kept in localStorage and sent as `Authorization: Bearer` on every
// request; GET /v1/whoami is the session check. When the server runs with no
// token configured (local passthrough mode), whoami succeeds with no header
// and the gate opens straight into the dashboard.

export default function AuthGate({ children }) {
  const [userCtx, setUserCtx] = useState(null);
  const [checking, setChecking] = useState(true);
  const [apiKey, setApiKey] = useState("");
  const [error, setError] = useState(null);

  const fetchWhoami = async (token) => {
    const headers = token ? { Authorization: `Bearer ${token}` } : {};
    const res = await fetch("/v1/whoami", { headers });
    if (!res.ok) return null;
    return res.json();
  };

  // Any ApiError(401) from the fetch layer dispatches `loomem:unauthorized`
  // (after clearing the stored token). Drop back to the login screen — a 401
  // is the only full-page error. This is the single place the app returns to
  // AuthGate from a live session.
  useEffect(() => {
    const onUnauthorized = () => {
      setUserCtx(null);
      setError("Your session expired. Please unlock again.");
      setChecking(false);
    };
    window.addEventListener("loomem:unauthorized", onUnauthorized);
    return () => window.removeEventListener("loomem:unauthorized", onUnauthorized);
  }, []);

  useEffect(() => {
    const init = async () => {
      try {
        const saved = localStorage.getItem("loomem_api_key");
        const ctx = await fetchWhoami(saved);
        if (ctx) {
          setUserCtx(ctx);
          return;
        }
        if (saved) localStorage.removeItem("loomem_api_key");
        // No/stale token — probe without a header. Succeeds when the server
        // runs in local passthrough mode (no token configured).
        const ctx2 = await fetchWhoami(null);
        if (ctx2) {
          setUserCtx(ctx2);
          return;
        }
      } catch {
        localStorage.removeItem("loomem_api_key");
      } finally {
        setChecking(false);
      }
    };
    init();
  }, []);

  const handleLogin = async () => {
    if (!apiKey.trim()) return;
    setError(null);
    try {
      const ctx = await fetchWhoami(apiKey.trim());
      if (ctx) {
        localStorage.setItem("loomem_api_key", apiKey.trim());
        setUserCtx(ctx);
      } else {
        setError("Invalid API token");
      }
    } catch {
      setError("Cannot connect to the server");
    }
  };

  if (checking) return <div className="h-screen w-full bg-[var(--bg)]" />;
  if (userCtx) {
    return typeof children === "function" ? children(userCtx) : children;
  }

  return (
    // Login screen on brand v2: warm canvas with the ambient weave-radial
    // wash, the living context-ring from the loomem.ai hero, the path-based
    // wordmark and a quiet panel card.
    <div
      className="h-screen w-full flex items-center justify-center"
      style={{ background: "var(--weave-radial)" }}
    >
      <div className="w-[340px] text-center">
        <div className="flex justify-center mb-2">
          <LoginHeroRing size={170} />
        </div>
        <img src="/wordmark.svg" alt="loomem" className="h-8 w-auto mx-auto" />
        <div className="text-[11px] text-[var(--text-muted)] mt-1.5 mb-7">dashboard</div>

        <div className="bg-[var(--panel)] border border-[var(--border)] rounded-2xl p-5 shadow-[var(--shadow-md)] text-left">
          <div className="space-y-3">
            <input
              type="password"
              value={apiKey}
              onChange={(e) => setApiKey(e.target.value)}
              onKeyDown={(e) => e.key === "Enter" && handleLogin()}
              placeholder="API token"
              autoFocus
              className="w-full bg-[var(--bg)] border border-[var(--border-strong)] rounded-lg px-4 py-3 text-sm text-[var(--text)] placeholder-[var(--text-subtle)] focus:outline-none focus:border-[var(--accent)] focus:ring-2 focus:ring-[var(--focus-ring)] font-mono"
            />
            <button
              onClick={handleLogin}
              className="w-full text-white rounded-full py-2.5 text-sm font-medium transition-all hover:-translate-y-[1px] shadow-[0_10px_30px_rgba(15,105,184,0.18)]"
              style={{ backgroundImage: "var(--weave-vivid)" }}
            >
              Unlock
            </button>
            {error && <div className="text-[var(--danger)] text-xs text-center">{error}</div>}
            <div className="text-[var(--text-subtle)] text-[11px] text-center pt-1">
              Paste this instance&apos;s API token to open the dashboard
            </div>
          </div>
        </div>
      </div>
    </div>
  );
}
