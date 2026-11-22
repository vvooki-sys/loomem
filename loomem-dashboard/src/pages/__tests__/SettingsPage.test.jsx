import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { act, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";

// The Dream card is the only interactive thing on Settings: it renders the
// schedule the server reports, edits it through PUT (re-rendering from the
// response), queues a run through POST, and never shows up for non-admins.

vi.mock("../../lib/api", () => ({
  getHealth: vi.fn(),
  getDreamSchedule: vi.fn(),
  putDreamSchedule: vi.fn(),
  runDreamScheduleNow: vi.fn(),
}));

import * as api from "../../lib/api";
import { ApiError } from "../../lib/apiError";
import SettingsPage from "../SettingsPage";

const ADMIN = { stream_id: "__user_abc", user_id: "u1", is_admin: true, role: "admin" };
const MEMBER = { stream_id: "__user_abc", user_id: "u1", is_admin: false, role: "member" };

const LAST_RUN = 1763851200;
const NEXT_RUN = 1763937600;

const SCHEDULE = {
  dream_enabled: true,
  scheduler_enabled: true,
  enabled: true,
  interval_secs: 86400,
  source: "config",
  last_run_at: LAST_RUN,
  next_run_at: NEXT_RUN,
  run_requested_at: null,
  poll_secs: 60,
  queued: false,
  last_result: {
    started_at: LAST_RUN,
    finished_at: LAST_RUN + 42,
    streams_total: 3,
    streams_failed: 1,
    chunks_processed: 120,
    facts_merged: 7,
    contradictions_resolved: 2,
    cost_usd: 0.0123,
    cost_cap_reached: 0,
    trigger: "schedule",
  },
};

// Expected local rendering of an epoch, computed independently of the card.
function localStamp(sec) {
  const d = new Date(sec * 1000);
  const p = (n) => String(n).padStart(2, "0");
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}`;
}

beforeEach(() => {
  const store = new Map([["loomem_api_key", "lm_live_test"]]);
  vi.stubGlobal("localStorage", {
    getItem: (k) => (store.has(k) ? store.get(k) : null),
    setItem: (k, v) => store.set(k, String(v)),
    removeItem: (k) => store.delete(k),
    clear: () => store.clear(),
  });
  api.getHealth.mockResolvedValue({ version: "0.26.0" });
  api.getDreamSchedule.mockResolvedValue(SCHEDULE);
});

afterEach(() => {
  vi.restoreAllMocks();
});

describe("SettingsPage — Dream card", () => {
  it("renders the schedule reported by GET", async () => {
    render(<SettingsPage userCtx={ADMIN} />);

    expect(await screen.findByRole("heading", { name: "Dream" })).toBeInTheDocument();
    await waitFor(() => expect(screen.getByRole("switch")).toHaveAttribute("aria-checked", "true"));
    expect(screen.getByRole("combobox")).toHaveValue("86400");
    expect(screen.getByText(localStamp(LAST_RUN))).toBeInTheDocument();
    expect(screen.getByText(localStamp(NEXT_RUN))).toBeInTheDocument();
    expect(
      screen.getByText("7 facts merged · 2 contradictions · $0.012 · 3 streams (1 failed)"),
    ).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Save" })).toBeDisabled();
    expect(screen.getByRole("button", { name: "Run now" })).toBeEnabled();
  });

  it("adds a preselected custom option when the server interval is not a preset", async () => {
    api.getDreamSchedule.mockResolvedValue({ ...SCHEDULE, interval_secs: 129600, source: "override" });
    render(<SettingsPage userCtx={ADMIN} />);

    await waitFor(() => expect(screen.getByRole("combobox")).toHaveValue("129600"));
    expect(screen.getByRole("option", { name: "custom (36 h)" })).toBeInTheDocument();
    expect(screen.getAllByRole("option")).toHaveLength(6);
  });

  it("Save sends PUT with the chosen interval and enabled flag, then re-renders from the response", async () => {
    api.putDreamSchedule.mockImplementation(async ({ enabled, interval_secs }) => ({
      ...SCHEDULE,
      enabled,
      interval_secs,
      source: "override",
      next_run_at: null,
    }));
    render(<SettingsPage userCtx={ADMIN} />);
    await waitFor(() => expect(screen.getByRole("switch")).toHaveAttribute("aria-checked", "true"));

    await userEvent.selectOptions(screen.getByRole("combobox"), "43200");
    await userEvent.click(screen.getByRole("switch"));
    expect(screen.getByRole("switch")).toHaveAttribute("aria-checked", "false");
    const save = screen.getByRole("button", { name: "Save" });
    expect(save).toBeEnabled();

    await userEvent.click(save);

    expect(api.putDreamSchedule).toHaveBeenCalledTimes(1);
    expect(api.putDreamSchedule).toHaveBeenCalledWith({ enabled: false, interval_secs: 43200 });
    await waitFor(() => expect(screen.getByRole("button", { name: "Save" })).toBeDisabled());
    expect(screen.getByRole("combobox")).toHaveValue("43200");
    expect(screen.getByRole("switch")).toHaveAttribute("aria-checked", "false");
    // next_run_at came back null → "—" (disabled schedule has no next run).
    expect(screen.getByText("—")).toBeInTheDocument();
    expect(screen.getByText("Dream schedule saved")).toBeInTheDocument();
  });

  it("surfaces a failed PUT as an error toast and keeps the draft", async () => {
    api.putDreamSchedule.mockRejectedValue(new ApiError(400, "interval_secs out of range"));
    render(<SettingsPage userCtx={ADMIN} />);
    await waitFor(() => expect(screen.getByRole("combobox")).toHaveValue("86400"));

    await userEvent.selectOptions(screen.getByRole("combobox"), "21600");
    await userEvent.click(screen.getByRole("button", { name: "Save" }));

    expect(await screen.findByText("interval_secs out of range (400)")).toBeInTheDocument();
    expect(screen.getByRole("combobox")).toHaveValue("21600");
    expect(screen.getByRole("button", { name: "Save" })).toBeEnabled();
  });

  it("Run now sends POST and shows the queued copy", async () => {
    const now = 1763900000;
    api.runDreamScheduleNow.mockResolvedValue({ ...SCHEDULE, run_requested_at: now, queued: true });
    render(<SettingsPage userCtx={ADMIN} />);
    const runNow = await screen.findByRole("button", { name: "Run now" });
    await waitFor(() => expect(runNow).toBeEnabled());

    await userEvent.click(runNow);

    expect(api.runDreamScheduleNow).toHaveBeenCalledTimes(1);
    expect(await screen.findByText("Queued — runs within a minute")).toBeInTheDocument();
    expect(screen.queryByText(localStamp(NEXT_RUN))).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Queued" })).toBeDisabled();
    expect(screen.getByText("Dream run queued")).toBeInTheDocument();
  });

  it("shows a 409 from Run now as an error toast", async () => {
    api.runDreamScheduleNow.mockRejectedValue(new ApiError(409, "dream worker is disabled"));
    render(<SettingsPage userCtx={ADMIN} />);
    const runNow = await screen.findByRole("button", { name: "Run now" });
    await waitFor(() => expect(runNow).toBeEnabled());

    await userEvent.click(runNow);

    expect(await screen.findByText("dream worker is disabled (409)")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Run now" })).toBeEnabled();
  });

  it("disables the controls and says so when dream is off in config", async () => {
    api.getDreamSchedule.mockResolvedValue({ ...SCHEDULE, dream_enabled: false, next_run_at: null });
    render(<SettingsPage userCtx={ADMIN} />);

    expect(await screen.findByText("Dream worker disabled in config")).toBeInTheDocument();
    expect(screen.getByRole("switch")).toBeDisabled();
    expect(screen.getByRole("combobox")).toBeDisabled();
    expect(screen.getByRole("button", { name: "Save" })).toBeDisabled();
    expect(screen.getByRole("button", { name: "Run now" })).toBeDisabled();
  });

  it("says when the scheduler is off in config", async () => {
    api.getDreamSchedule.mockResolvedValue({ ...SCHEDULE, scheduler_enabled: false });
    render(<SettingsPage userCtx={ADMIN} />);

    expect(await screen.findByText("Scheduler disabled in config")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Run now" })).toBeDisabled();
  });

  it("says when the dream worker is paused and disables Run now", async () => {
    api.getDreamSchedule.mockResolvedValue({ ...SCHEDULE, paused: true });
    render(<SettingsPage userCtx={ADMIN} />);

    expect(await screen.findByText("Dream worker is paused")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Run now" })).toBeDisabled();
    // Pause only blocks runs — the schedule itself stays editable.
    expect(screen.getByRole("switch")).toBeEnabled();
    expect(screen.getByRole("combobox")).toBeEnabled();
  });

  it("shows the paused 409 from Run now as an error toast", async () => {
    api.runDreamScheduleNow.mockRejectedValue(new ApiError(409, "Dream worker is paused"));
    render(<SettingsPage userCtx={ADMIN} />);
    const runNow = await screen.findByRole("button", { name: "Run now" });
    await waitFor(() => expect(runNow).toBeEnabled());

    await userEvent.click(runNow);

    expect(await screen.findByText("Dream worker is paused (409)")).toBeInTheDocument();
  });

  it.each([
    ["deadline", "Stopped at deadline · "],
    ["failed", "Failed · "],
  ])("prefixes the last-result line when status is %s", async (status, prefix) => {
    api.getDreamSchedule.mockResolvedValue({
      ...SCHEDULE,
      last_result: { ...SCHEDULE.last_result, status },
    });
    render(<SettingsPage userCtx={ADMIN} />);

    expect(
      await screen.findByText(`${prefix}7 facts merged · 2 contradictions · $0.012 · 3 streams (1 failed)`),
    ).toBeInTheDocument();
  });

  it("shows no prefix when status is ok", async () => {
    api.getDreamSchedule.mockResolvedValue({
      ...SCHEDULE,
      last_result: { ...SCHEDULE.last_result, status: "ok" },
    });
    render(<SettingsPage userCtx={ADMIN} />);

    expect(
      await screen.findByText("7 facts merged · 2 contradictions · $0.012 · 3 streams (1 failed)"),
    ).toBeInTheDocument();
  });

  it("hides the Dream card from non-admins", async () => {
    render(<SettingsPage userCtx={MEMBER} />);

    expect(await screen.findByText("v0.26.0")).toBeInTheDocument();
    expect(screen.queryByRole("heading", { name: "Dream" })).not.toBeInTheDocument();
    expect(screen.queryByRole("switch")).not.toBeInTheDocument();
    expect(api.getDreamSchedule).not.toHaveBeenCalled();
  });

  it("shows a GET failure inside the card without breaking the rest of the page", async () => {
    api.getDreamSchedule.mockRejectedValue(new ApiError(500, "boom"));
    render(<SettingsPage userCtx={ADMIN} />);

    expect(await screen.findByRole("alert")).toHaveTextContent("Couldn't load the dream schedule. boom (500)");
    expect(screen.getByText("__user_abc")).toBeInTheDocument();
    expect(await screen.findByText("v0.26.0")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Log out" })).toBeInTheDocument();
    expect(screen.getByRole("switch")).toBeDisabled();
    expect(screen.getByRole("button", { name: "Run now" })).toBeDisabled();
  });
});

describe("SettingsPage — queued polling", () => {
  const QUEUED = { ...SCHEDULE, run_requested_at: 1763900000, queued: true };
  const CONSUMED = { ...SCHEDULE, last_run_at: 1763900030, run_requested_at: null, queued: false };

  beforeEach(() => {
    vi.useFakeTimers();
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  // Flushes the pending mock promises (and any timers due within `ms`).
  const tick = (ms) => act(() => vi.advanceTimersByTimeAsync(ms));

  it("re-GETs every 15 s while queued and stops once run_requested_at is null", async () => {
    api.getDreamSchedule
      .mockResolvedValueOnce(QUEUED)
      .mockResolvedValueOnce(QUEUED)
      .mockResolvedValue(CONSUMED);
    render(<SettingsPage userCtx={ADMIN} />);

    await tick(0);
    expect(api.getDreamSchedule).toHaveBeenCalledTimes(1);
    expect(screen.getByText("Queued — runs within a minute")).toBeInTheDocument();

    await tick(14_999);
    expect(api.getDreamSchedule).toHaveBeenCalledTimes(1);

    await tick(1);
    expect(api.getDreamSchedule).toHaveBeenCalledTimes(2);
    expect(screen.getByText("Queued — runs within a minute")).toBeInTheDocument();

    await tick(15_000);
    expect(api.getDreamSchedule).toHaveBeenCalledTimes(3);
    expect(screen.queryByText("Queued — runs within a minute")).not.toBeInTheDocument();
    expect(screen.getByText(localStamp(1763900030))).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Run now" })).toBeEnabled();

    // Polling has stopped: no further GETs.
    await tick(60_000);
    expect(api.getDreamSchedule).toHaveBeenCalledTimes(3);
  });

  it("clears the polling interval on unmount", async () => {
    api.getDreamSchedule.mockResolvedValue(QUEUED);
    const { unmount } = render(<SettingsPage userCtx={ADMIN} />);

    await tick(0);
    expect(api.getDreamSchedule).toHaveBeenCalledTimes(1);
    await tick(15_000);
    expect(api.getDreamSchedule).toHaveBeenCalledTimes(2);

    unmount();
    await tick(60_000);
    expect(api.getDreamSchedule).toHaveBeenCalledTimes(2);
  });
});
