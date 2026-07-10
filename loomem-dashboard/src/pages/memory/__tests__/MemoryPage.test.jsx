import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { render, screen, fireEvent, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter } from "react-router-dom";

// MemoryPage master-detail integration, single-user: server-side substring
// search, semantic mode + error surface, stream banner, edit feedback,
// delete, entities in-panel with no route change.

vi.mock("../../../lib/api", () => ({
  getMemory: vi.fn(),
  searchMemories: vi.fn(),
  updateMemory: vi.fn(),
  deleteMemory: vi.fn(),
  getMemoryChain: vi.fn(),
}));

import * as api from "../../../lib/api";
import { ApiError } from "../../../lib/apiError";
import MemoryPage from "../../MemoryPage";

const MEMORY = {
  id: "m1",
  content: "original memory text",
  layer: "L0",
  confidence: 0.9,
  decay: 0.01,
  event_date: "2026-01-01",
  created_at: 0,
  source_agent: "cc",
  version: 1,
  entity_ids: ["ent12345678"],
};

const ONE = { items: [MEMORY], total: 1, page: 1, per_page: 50 };

const CTX = { stream_id: "__user_default__", is_admin: true, role: "Admin", user_id: null };

function renderPage(userCtx = CTX) {
  return render(
    <MemoryRouter initialEntries={["/memory"]}>
      <MemoryPage userCtx={userCtx} />
    </MemoryRouter>,
  );
}

beforeEach(() => {
  vi.clearAllMocks();
  api.getMemory.mockResolvedValue(ONE);
  api.searchMemories.mockResolvedValue({ results: [] });
});
afterEach(() => vi.restoreAllMocks());

describe("stream banner", () => {
  it("renders stream · N memories", async () => {
    renderPage();
    await waitFor(() => expect(screen.getByText("__user_default__")).toBeInTheDocument());
    expect(screen.getByText(/1 memories/)).toBeInTheDocument();
  });
});

describe("server-side search", () => {
  it("substring: typing calls /api/dashboard/memory with q=", async () => {
    renderPage();
    await waitFor(() => expect(screen.getAllByText("original memory text").length).toBeGreaterThan(0));
    const input = screen.getByPlaceholderText(/Filter memories/i);
    await userEvent.type(input, "hello");
    await waitFor(() =>
      expect(api.getMemory).toHaveBeenCalledWith(expect.objectContaining({ q: "hello" })),
    );
  });

  it("semantic mode calls POST /v1/search; a failure surfaces an error", async () => {
    api.searchMemories.mockRejectedValue(new ApiError(500, "semantic down"));
    renderPage();
    await waitFor(() => expect(screen.getAllByText("original memory text").length).toBeGreaterThan(0));

    await userEvent.click(screen.getByRole("tab", { name: /Semantic/i }));
    await userEvent.type(screen.getByPlaceholderText(/Search by meaning/i), "vibe");

    await waitFor(() => expect(api.searchMemories).toHaveBeenCalledWith("vibe", expect.any(Object)));
    await waitFor(() => expect(screen.getByText(/Couldn't load this panel/i)).toBeInTheDocument());
  });

  it("semantic mode returns all layers and hides the layer control", async () => {
    // /v1/search has no layer param; semantic must NOT client-side filter the
    // ranked window (would starve matches ranked below top_k). The layer chips
    // are hidden and reset to "all" instead.
    api.searchMemories.mockResolvedValue({
      results: [
        { id: "s0", content: "raw semantic hit", score: 0.9, metadata: { level: 0 } },
        { id: "s1", content: "consolidated semantic hit", score: 0.8, metadata: { level: 1 } },
      ],
    });
    renderPage();
    await waitFor(() => expect(screen.getAllByText("original memory text").length).toBeGreaterThan(0));

    // Layer chip exists in substring mode…
    expect(screen.getByRole("button", { name: "L1" })).toBeInTheDocument();
    await userEvent.click(screen.getByRole("button", { name: "L1" }));
    await userEvent.click(screen.getByRole("tab", { name: /Semantic/i }));
    // …and is hidden in semantic mode.
    expect(screen.queryByRole("button", { name: "L1" })).not.toBeInTheDocument();

    await userEvent.type(screen.getByPlaceholderText(/Search by meaning/i), "hit");
    await waitFor(() => expect(screen.getByText("raw semantic hit")).toBeInTheDocument());
    expect(screen.getByText("consolidated semantic hit")).toBeInTheDocument();
  });
});

describe("edit feedback", () => {
  it("200 → success toast; edit UI closes", async () => {
    api.updateMemory.mockResolvedValue({ id: "m1" });
    renderPage();
    await waitFor(() => expect(screen.getAllByText("original memory text").length).toBeGreaterThan(0));

    fireEvent.click(screen.getAllByText("original memory text")[0]); // select row
    await userEvent.click(await screen.findByRole("button", { name: /^Edit$/i }));
    const ta = screen.getByRole("textbox", { name: /Edit memory content/i });
    await userEvent.clear(ta);
    await userEvent.type(ta, "edited text");
    await userEvent.click(screen.getByRole("button", { name: /Save/i }));

    await waitFor(() => expect(api.updateMemory).toHaveBeenCalledWith("m1", { content: "edited text" }));
    await waitFor(() => expect(screen.getByText("Memory updated")).toBeInTheDocument());
  });

  it("500 → error toast; typed text stays in the editor", async () => {
    api.updateMemory.mockRejectedValue(new ApiError(500, "boom"));
    renderPage();
    await waitFor(() => expect(screen.getAllByText("original memory text").length).toBeGreaterThan(0));

    fireEvent.click(screen.getAllByText("original memory text")[0]);
    await userEvent.click(await screen.findByRole("button", { name: /^Edit$/i }));
    const ta = screen.getByRole("textbox", { name: /Edit memory content/i });
    await userEvent.clear(ta);
    await userEvent.type(ta, "edited text");
    await userEvent.click(screen.getByRole("button", { name: /Save/i }));

    await waitFor(() => expect(screen.getByText(/boom \(500\)/i)).toBeInTheDocument());
    expect(screen.getByRole("textbox", { name: /Edit memory content/i })).toHaveValue("edited text");
  });

  it("content edit that supersedes to a new id refreshes then follows it", async () => {
    api.updateMemory.mockResolvedValue({ id: "m2" });
    const NEW = { ...MEMORY, id: "m2", content: "edited text", version: 2 };
    api.getMemory
      .mockResolvedValueOnce(ONE) // initial load
      .mockResolvedValue({ items: [NEW], total: 1, page: 1, per_page: 50 }); // post-edit refresh
    renderPage();
    await waitFor(() => expect(screen.getAllByText("original memory text").length).toBeGreaterThan(0));

    fireEvent.click(screen.getAllByText("original memory text")[0]);
    await userEvent.click(await screen.findByRole("button", { name: /^Edit$/i }));
    const ta = screen.getByRole("textbox", { name: /Edit memory content/i });
    await userEvent.clear(ta);
    await userEvent.type(ta, "edited text");
    await userEvent.click(screen.getByRole("button", { name: /Save/i }));

    // The new version is in the list and selected in the detail panel.
    await waitFor(() => expect(screen.getAllByText("edited text").length).toBeGreaterThan(1));
  });
});

describe("delete", () => {
  it("soft-deletes, toasts, and the row leaves the list after refresh", async () => {
    api.deleteMemory.mockResolvedValue(null);
    // First list load returns the row; the post-delete refresh returns empty.
    api.getMemory.mockResolvedValueOnce(ONE).mockResolvedValue({ items: [], total: 0 });
    renderPage();
    await waitFor(() => expect(screen.getAllByText("original memory text").length).toBeGreaterThan(0));

    fireEvent.click(screen.getAllByText("original memory text")[0]);
    await userEvent.click(await screen.findByRole("button", { name: /^Delete$/i }));
    await userEvent.click(screen.getByRole("button", { name: /Confirm delete/i }));

    await waitFor(() => expect(api.deleteMemory).toHaveBeenCalledWith("m1"));
    await waitFor(() => expect(screen.getByText("Memory deleted")).toBeInTheDocument());
    await waitFor(() => expect(screen.queryByText("original memory text")).not.toBeInTheDocument());
  });
});

describe("entities in-panel", () => {
  it("clicking an entity fetches its memories via entity_id, without leaving /memory", async () => {
    renderPage();
    await waitFor(() => expect(screen.getAllByText("original memory text").length).toBeGreaterThan(0));

    fireEvent.click(screen.getAllByText("original memory text")[0]);
    await userEvent.click(await screen.findByRole("button", { name: /Entities/i }));
    await userEvent.click(screen.getByRole("button", { name: /ent12345/i }));

    await waitFor(() =>
      expect(api.getMemory).toHaveBeenCalledWith(expect.objectContaining({ entity_id: "ent12345678" })),
    );
    // In-panel navigation only — a breadcrumb back, no graph/route change.
    expect(screen.getByRole("button", { name: /Back to memory/i })).toBeInTheDocument();
  });
});
