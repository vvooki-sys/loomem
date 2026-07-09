import { describe, it, expect, vi } from "vitest";
import { render, screen, fireEvent } from "@testing-library/react";
import ErrorState from "../ErrorState";
import { ApiError } from "../../../lib/apiError";

// Cycle/152 AC-4 — ErrorState renders disjointly from empty, 403 has no Retry,
// and Retry invokes the callback.

describe("ErrorState (AC-4)", () => {
  it("shows the message and a Retry button for a 500", () => {
    const onRetry = vi.fn();
    render(<ErrorState error={new ApiError(500, "boom")} onRetry={onRetry} />);
    expect(screen.getByText(/Couldn't load this panel/i)).toBeInTheDocument();
    expect(screen.getByText(/boom \(500\)/i)).toBeInTheDocument();
    const btn = screen.getByRole("button", { name: /Retry/i });
    fireEvent.click(btn);
    expect(onRetry).toHaveBeenCalledTimes(1);
  });

  it("403 shows a permission message and NO Retry button", () => {
    render(<ErrorState error={new ApiError(403, "nope")} onRetry={vi.fn()} />);
    expect(screen.getByText(/don't have permission/i)).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /Retry/i })).not.toBeInTheDocument();
  });

  it("omits Retry when no onRetry is supplied", () => {
    render(<ErrorState error={new ApiError(500, "boom")} />);
    expect(screen.queryByRole("button", { name: /Retry/i })).not.toBeInTheDocument();
  });
});
