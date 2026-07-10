import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { render, screen, waitFor } from "@testing-library/react";

vi.mock("../../../lib/api", () => ({
  getMemoryChain: vi.fn(),
}));

import * as api from "../../../lib/api";
import { ApiError } from "../../../lib/apiError";
import MemoryHistoryTab from "../MemoryHistoryTab";

beforeEach(() => vi.clearAllMocks());
afterEach(() => vi.restoreAllMocks());

// Cycle/163 S4 / AC-8 — history renders the chain; a failed fetch is an
// ErrorState, never an eternal spinner; empty is explicit copy.

describe("MemoryHistoryTab (AC-8)", () => {
  it("renders the version chain", async () => {
    api.getMemoryChain.mockResolvedValue({
      chain: [{ id: "v1", version: 1, content: "first version", is_latest: true, timestamp: 0 }],
    });
    render(<MemoryHistoryTab chunkId="m1" />);
    await waitFor(() => expect(screen.getByText("first version")).toBeInTheDocument());
    expect(screen.getByText(/v1/)).toBeInTheDocument();
  });

  it("failed chain fetch → ErrorState, not a spinner", async () => {
    api.getMemoryChain.mockRejectedValue(new ApiError(500, "no chain"));
    render(<MemoryHistoryTab chunkId="m1" />);
    await waitFor(() =>
      expect(screen.getByText(/Couldn't load this panel/i)).toBeInTheDocument(),
    );
  });

  it("empty chain → explicit empty copy", async () => {
    api.getMemoryChain.mockResolvedValue({ chain: [] });
    render(<MemoryHistoryTab chunkId="m1" />);
    await waitFor(() => expect(screen.getByText(/No recorded versions/i)).toBeInTheDocument());
  });

  // Cycle/173 S3 / AC-4 — a diff between adjacent versions surfaces the added
  // and removed words. First version has no predecessor → no diff block.
  it("renders a word-level diff between two versions", async () => {
    api.getMemoryChain.mockResolvedValue({
      chain: [
        { id: "v1", version: 1, content: "the cat sat", is_latest: false, timestamp: 0 },
        { id: "v2", version: 2, content: "the dog sat", is_latest: true, timestamp: 10 },
      ],
    });
    const { container } = render(<MemoryHistoryTab chunkId="v1" />);
    await waitFor(() => expect(screen.getByText(/Changes from previous/i)).toBeInTheDocument());

    // Exactly one diff block (only v2 has a predecessor).
    expect(screen.getAllByText(/Changes from previous/i)).toHaveLength(1);

    // Removed token struck through (danger), added token tinted (success).
    const removed = [...container.querySelectorAll("span.line-through")];
    expect(removed.some((el) => el.textContent.includes("cat"))).toBe(true);
    const added = [...container.querySelectorAll("span")].filter((el) =>
      el.className.includes("--success"),
    );
    expect(added.some((el) => el.textContent.includes("dog"))).toBe(true);
  });
});
