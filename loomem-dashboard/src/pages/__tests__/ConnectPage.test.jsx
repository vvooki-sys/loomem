import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";

// Connect is the onboarding surface: everything on it must be REAL — the
// endpoint comes from the page origin, the token from the unlock session,
// the recipes are parameterized with both, and the status light is a live
// probe (mocked here at the api layer).

vi.mock("../../lib/api", () => ({
  getMemoryStatus: vi.fn(),
}));

import * as api from "../../lib/api";
import { ApiError } from "../../lib/apiError";
import ConnectPage from "../ConnectPage";

const TOKEN = "lm_live_7c3d9f21ba64e0d8a1552e3f9a";

beforeEach(() => {
  const store = new Map([["loomem_api_key", TOKEN]]);
  vi.stubGlobal("localStorage", {
    getItem: (k) => (store.has(k) ? store.get(k) : null),
    setItem: (k, v) => store.set(k, String(v)),
    removeItem: (k) => store.delete(k),
    clear: () => store.clear(),
  });
  api.getMemoryStatus.mockResolvedValue({ total_chunks: 248 });
});

afterEach(() => {
  vi.restoreAllMocks();
});

describe("ConnectPage", () => {
  it("shows the real MCP endpoint derived from the page origin", async () => {
    render(<ConnectPage />);
    const endpoint = `${window.location.origin}/mcp`;
    await waitFor(() => expect(screen.getByText(endpoint)).toBeInTheDocument());
  });

  it("masks the token until Reveal is clicked", async () => {
    render(<ConnectPage />);
    expect(screen.queryByText(TOKEN)).not.toBeInTheDocument();
    await userEvent.click(screen.getByRole("button", { name: /Reveal/i }));
    expect(screen.getByText(TOKEN)).toBeInTheDocument();
    await userEvent.click(screen.getByRole("button", { name: /Hide/i }));
    expect(screen.queryByText(TOKEN)).not.toBeInTheDocument();
  });

  it("parameterizes each client recipe with the real endpoint", async () => {
    render(<ConnectPage />);
    const endpoint = `${window.location.origin}/mcp`;

    // Default tab: Claude Desktop JSON. The endpoint appears both in the
    // endpoint field and inside the recipe body.
    expect(screen.getByText(/claude_desktop_config\.json/)).toBeInTheDocument();
    expect(
      screen.getAllByText(new RegExp(endpoint.replace(/[/.]/g, "\\$&"))).length,
    ).toBeGreaterThanOrEqual(2);

    await userEvent.click(screen.getByRole("tab", { name: "Claude Code" }));
    expect(screen.getByText(/claude mcp add --transport http loomem/)).toBeInTheDocument();

    await userEvent.click(screen.getByRole("tab", { name: "Cursor" }));
    expect(screen.getByText(/\.cursor\/mcp\.json/)).toBeInTheDocument();

    await userEvent.click(screen.getByRole("tab", { name: "ChatGPT" }));
    expect(screen.getByText(/Connectors/)).toBeInTheDocument();
  });

  it("live status turns green when the engine answers", async () => {
    render(<ConnectPage />);
    await waitFor(() =>
      expect(screen.getByText(/Connected — this instance is live/i)).toBeInTheDocument(),
    );
    expect(api.getMemoryStatus).toHaveBeenCalled();
    expect(screen.getByText(/248 memories/)).toBeInTheDocument();
  });

  it("live status turns red when the probe fails — no fabricated green", async () => {
    api.getMemoryStatus.mockRejectedValue(new ApiError(0, "Network error"));
    render(<ConnectPage />);
    await waitFor(() => expect(screen.getByText(/Unreachable/i)).toBeInTheDocument());
    expect(screen.getByText(/Probe failed/i)).toBeInTheDocument();
  });

  it("explains local mode when no token is stored", async () => {
    localStorage.removeItem("loomem_api_key");
    render(<ConnectPage />);
    expect(await screen.findByText(/without a token \(local mode\)/i)).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /Reveal/i })).not.toBeInTheDocument();
  });
});
