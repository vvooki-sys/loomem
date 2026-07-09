import { useEffect, useState } from "react";
import { Settings as SettingsIcon } from "lucide-react";
import { getHealth } from "../lib/api";

// Minimal v1 settings: what this instance is (stream, role, version) and the
// way out (logout). Token management happens at the deployment level in a
// single-user instance — there is nothing to manage here yet.

export default function SettingsPage({ userCtx }) {
  const [version, setVersion] = useState(null);

  useEffect(() => {
    // Decorative version readout — a failed /health fetch just hides it.
    getHealth()
      .then((d) => setVersion(d?.version ?? null))
      .catch(() => {});
  }, []);

  const logout = () => {
    localStorage.removeItem("loomem_api_key");
    window.location.reload();
  };

  return (
    <div className="h-full w-full overflow-auto bg-[var(--bg)] text-[var(--text)]">
      <div className="max-w-2xl mx-auto px-8 py-12">
        <div className="flex items-center gap-3 mb-8">
          <SettingsIcon size={22} className="text-[var(--text-muted)]" />
          <h1 className="font-display text-2xl font-semibold">Settings</h1>
        </div>

        <div className="bg-[var(--panel)] border border-[var(--border)] rounded-lg divide-y divide-[var(--border)]">
          <div className="px-5 py-4">
            <div className="text-[12px] uppercase tracking-wide text-[var(--text-muted)] mb-1">Stream ID</div>
            <div className="font-mono text-sm">{userCtx?.stream_id || "—"}</div>
          </div>
          <div className="px-5 py-4">
            <div className="text-[12px] uppercase tracking-wide text-[var(--text-muted)] mb-1">Role</div>
            <div className="text-sm">{userCtx?.role || "—"}</div>
          </div>
          <div className="px-5 py-4">
            <div className="text-[12px] uppercase tracking-wide text-[var(--text-muted)] mb-1">Server version</div>
            <div className="font-mono text-sm">{version ? `v${version}` : "—"}</div>
          </div>
        </div>

        <div className="mt-6">
          <button
            onClick={logout}
            className="px-4 py-2 rounded-full border border-[var(--border-strong)] text-sm hover:bg-[var(--row-hover)]"
          >
            Log out
          </button>
        </div>
      </div>
    </div>
  );
}
