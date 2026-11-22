import { useEffect, useState } from "react";
import { getDreamSchedule, putDreamSchedule, runDreamScheduleNow } from "../../lib/api";
import { errorCopy } from "../../lib/errorCopy";

// Admin-only "Dream" card on the Settings page: the scheduled dream pass
// (default once a day) is edited here, not in config.toml. The server row is
// the single source of truth — every mutation re-renders from the response
// (PUT returns the same shape as GET; POST .../run returns it with
// `queued: true`). A queued run is executed by the scheduler at its next
// poll (≤ `poll_secs`), so while a run is queued the card re-fetches until
// the server reports the request consumed.

const PRESETS = [
  { value: 21600, label: "6 h" },
  { value: 43200, label: "12 h" },
  { value: 86400, label: "24 h" },
  { value: 172800, label: "48 h" },
  { value: 604800, label: "7 d" },
];

const QUEUED_POLL_MS = 15_000;

// Epoch seconds → local "YYYY-MM-DD HH:mm"; "—" when unknown.
export function fmtEpoch(sec) {
  if (typeof sec !== "number" || !Number.isFinite(sec) || sec <= 0) return "—";
  const d = new Date(sec * 1000);
  const pad = (n) => String(n).padStart(2, "0");
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}`;
}

function fmtLastResult(r) {
  if (!r) return null;
  const n = (v) => (typeof v === "number" ? v : 0);
  const cost = n(r.cost_usd).toFixed(3);
  return `${n(r.facts_merged)} facts merged · ${n(r.contradictions_resolved)} contradictions · $${cost} · ${n(r.streams_total)} streams (${n(r.streams_failed)} failed)`;
}

function draftFrom(schedule) {
  return {
    enabled: schedule?.enabled === true,
    intervalSecs: typeof schedule?.interval_secs === "number" ? schedule.interval_secs : 86400,
  };
}

function intervalOptions(serverSecs) {
  if (PRESETS.some((p) => p.value === serverSecs)) return PRESETS;
  const hours = Math.round((serverSecs / 3600) * 10) / 10;
  return [...PRESETS, { value: serverSecs, label: `custom (${hours} h)` }];
}

function isQueued(schedule) {
  return schedule?.queued === true || schedule?.run_requested_at != null;
}

function Row({ label, children }) {
  return (
    <div className="px-5 py-4">
      <div className="text-[12px] uppercase tracking-wide text-(--text-muted) mb-1">{label}</div>
      {children}
    </div>
  );
}

export default function DreamScheduleCard({ push }) {
  const [schedule, setSchedule] = useState(null);
  const [loadError, setLoadError] = useState(null);
  const [draft, setDraft] = useState(() => draftFrom(null));
  const [saving, setSaving] = useState(false);
  const [queuing, setQueuing] = useState(false);

  // Initial load. A failure is shown inside the card only — the rest of the
  // Settings page keeps working.
  useEffect(() => {
    let cancelled = false;
    getDreamSchedule()
      .then((data) => {
        if (cancelled) return;
        setSchedule(data);
        setDraft(draftFrom(data));
        setLoadError(null);
      })
      .catch((err) => {
        if (!cancelled) setLoadError(err);
      });
    return () => {
      cancelled = true;
    };
  }, []);

  // While a run is queued, re-fetch until the scheduler has picked it up
  // (the server clears `run_requested_at` when the pass starts).
  const queued = isQueued(schedule);
  useEffect(() => {
    if (!queued) return undefined;
    const timer = window.setInterval(() => {
      getDreamSchedule()
        .then((data) => setSchedule(data))
        .catch(() => {
          // Transient poll failure — keep the queued state and retry.
        });
    }, QUEUED_POLL_MS);
    return () => window.clearInterval(timer);
  }, [queued]);

  const dreamOff = schedule?.dream_enabled === false;
  const schedulerOff = schedule?.scheduler_enabled === false;
  const dirty =
    schedule != null &&
    (draft.enabled !== (schedule.enabled === true) || draft.intervalSecs !== schedule.interval_secs);
  const controlsDisabled = schedule == null || dreamOff || saving;

  const save = async () => {
    setSaving(true);
    try {
      const data = await putDreamSchedule({
        enabled: draft.enabled,
        interval_secs: draft.intervalSecs,
      });
      setSchedule(data);
      setDraft(draftFrom(data));
      push?.({ tone: "success", message: "Dream schedule saved" });
    } catch (err) {
      push?.({ tone: "error", message: errorCopy(err) });
    } finally {
      setSaving(false);
    }
  };

  const runNow = async () => {
    setQueuing(true);
    try {
      const data = await runDreamScheduleNow();
      setSchedule(data);
      push?.({ tone: "success", message: "Dream run queued" });
    } catch (err) {
      push?.({ tone: "error", message: errorCopy(err) });
    } finally {
      setQueuing(false);
    }
  };

  let statusLine = null;
  if (loadError) {
    statusLine = { tone: "danger", text: `Couldn't load the dream schedule. ${errorCopy(loadError)}` };
  } else if (dreamOff) {
    statusLine = { tone: "warn", text: "Dream worker disabled in config" };
  } else if (schedulerOff) {
    statusLine = { tone: "warn", text: "Scheduler disabled in config" };
  }

  const lastResult = fmtLastResult(schedule?.last_result);

  return (
    <section
      aria-labelledby="dream-schedule-heading"
      className="mt-6 bg-(--panel) border border-(--border) rounded-lg divide-y divide-(--border)"
    >
      <div className="px-5 py-4">
        <h2 id="dream-schedule-heading" className="font-display text-base font-semibold">
          Dream
        </h2>
        <p className="text-[13px] text-(--text-muted) mt-1">
          Scheduled consolidation pass over every active stream.
        </p>
        {statusLine && (
          <p
            role={statusLine.tone === "danger" ? "alert" : "status"}
            className={`text-[13px] mt-2 ${statusLine.tone === "danger" ? "text-(--danger)" : "text-(--warn)"}`}
          >
            {statusLine.text}
          </p>
        )}
      </div>

      <div className="px-5 py-4 flex items-center justify-between gap-4">
        <span id="dream-auto-label" className="text-sm">
          Run dream automatically
        </span>
        <button
          type="button"
          role="switch"
          aria-checked={draft.enabled}
          aria-labelledby="dream-auto-label"
          disabled={controlsDisabled}
          onClick={() => setDraft((d) => ({ ...d, enabled: !d.enabled }))}
          className={`relative inline-flex h-5 w-9 shrink-0 rounded-full transition-colors disabled:opacity-50 ${
            draft.enabled ? "bg-(--accent)" : "bg-(--border-strong)"
          }`}
        >
          <span
            className={`absolute top-0.5 left-0.5 h-4 w-4 rounded-full bg-white shadow-xs transition-transform ${
              draft.enabled ? "translate-x-4" : ""
            }`}
          />
        </button>
      </div>

      <div className="px-5 py-4">
        <label className="flex items-center gap-1.5 text-xs text-(--text-muted)">
          Every
          <select
            value={draft.intervalSecs}
            disabled={controlsDisabled}
            onChange={(e) => setDraft((d) => ({ ...d, intervalSecs: Number(e.target.value) }))}
            className="bg-(--bg) border border-(--border) rounded-md px-2 py-1 text-xs text-(--text) focus:outline-hidden focus:border-(--accent) disabled:opacity-50"
          >
            {intervalOptions(schedule?.interval_secs ?? draft.intervalSecs).map((o) => (
              <option key={o.value} value={o.value}>
                {o.label}
              </option>
            ))}
          </select>
        </label>
      </div>

      <Row label="Last run">
        <div className="font-mono text-sm">{fmtEpoch(schedule?.last_run_at)}</div>
        {lastResult && <div className="text-[13px] text-(--text-muted) mt-1">{lastResult}</div>}
      </Row>

      <Row label="Next run">
        <div className="font-mono text-sm">
          {queued ? "Queued — runs within a minute" : fmtEpoch(schedule?.next_run_at)}
        </div>
      </Row>

      <div className="px-5 py-4 flex items-center gap-2">
        <button
          type="button"
          onClick={save}
          disabled={!dirty || controlsDisabled}
          className="px-3 py-1.5 rounded-full text-xs bg-(--accent) text-white hover:bg-(--accent-hover) disabled:opacity-50"
        >
          {saving ? "Saving…" : "Save"}
        </button>
        <button
          type="button"
          onClick={runNow}
          disabled={schedule == null || dreamOff || schedulerOff || queued || queuing}
          className="px-3 py-1.5 rounded-full text-xs border border-(--border-strong) hover:bg-(--row-hover) disabled:opacity-50"
        >
          {queuing ? "Queuing…" : queued ? "Queued" : "Run now"}
        </button>
      </div>
    </section>
  );
}
