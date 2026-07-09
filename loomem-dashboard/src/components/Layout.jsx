import { useEffect, useState } from "react";
import { NavLink, Outlet } from "react-router-dom";
import { Brain, Cable, Settings } from "lucide-react";

// Minimal v1 shell: Connect / Memory / Settings. Connect is the first item —
// it's the onboarding surface of a hosted single-user instance.
const NAV_ITEMS = [
  { to: "/connect", label: "Connect", icon: Cable, end: false },
  { to: "/memory", label: "Memory", icon: Brain, end: false },
  { to: "/settings", label: "Settings", icon: Settings, end: false },
];

export default function Layout({ userCtx }) {
  const streamLabel = userCtx?.stream_id || "local";
  const [version, setVersion] = useState(null);

  useEffect(() => {
    // The footer version string is a decorative nicety, NOT a data panel — a
    // failed /health fetch just hides the version. Swallowing the error here
    // is intentional (no ErrorState).
    fetch("/health")
      .then((r) => r.json())
      .then((d) => setVersion(d.version))
      .catch(() => {});
  }, []);

  const handleLogout = () => {
    localStorage.removeItem("loomem_api_key");
    window.location.reload();
  };

  return (
    <div className="flex h-screen w-screen bg-[var(--bg)] text-[var(--text)] overflow-hidden">
      <aside className="w-[220px] shrink-0 bg-[var(--panel)] border-r border-[var(--border)] flex flex-col">
        <div className="px-5 py-4 border-b border-[var(--border)]">
          {/* Canonical wordmark asset from the brand pack: text as paths, so
              it renders identically before the Fraunces webfont loads.
              Static (no flow) — the app surface stays calm. */}
          <img src="/wordmark.svg" alt="loomem" className="h-6 w-auto" />
          <div className="text-[11px] text-[var(--text-muted)] truncate" title={streamLabel}>
            {streamLabel}
          </div>
          <button
            onClick={handleLogout}
            className="text-[10px] text-[var(--text-subtle)] hover:text-[var(--text)] mt-1.5 transition-colors focus-visible:outline focus-visible:outline-2 focus-visible:outline-[var(--focus-ring)]"
            title="Log out"
          >
            logout
          </button>
        </div>

        <nav className="flex-1 overflow-y-auto px-2.5 py-3 space-y-[3px]">
          {NAV_ITEMS.map(({ to, label, icon: Icon, end }) => (
            <NavLink
              key={to}
              to={to}
              end={end}
              className={({ isActive }) =>
                `flex items-center gap-2.5 px-3 py-2 rounded-full text-sm transition-colors focus-visible:outline focus-visible:outline-2 focus-visible:outline-[var(--focus-ring)] ${
                  isActive
                    ? "bg-[var(--row-selected)] text-[var(--accent)] font-medium"
                    : "text-[var(--text-muted)] hover:bg-[var(--row-hover)] hover:text-[var(--text)]"
                }`
              }
            >
              <Icon size={16} strokeWidth={1.75} />
              <span>{label}</span>
            </NavLink>
          ))}
        </nav>

        <div className="px-3 py-3 border-t border-[var(--border)] text-[10px] text-[var(--text-subtle)]">
          loomem dashboard{version ? ` v${version}` : ""}
        </div>
      </aside>

      <main className="flex-1 min-w-0 overflow-hidden">
        <Outlet />
      </main>
    </div>
  );
}
