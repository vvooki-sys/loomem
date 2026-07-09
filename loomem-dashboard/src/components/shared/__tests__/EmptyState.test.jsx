import { describe, it, expect } from "vitest";
import { render, screen } from "@testing-library/react";
import EmptyState from "../EmptyState";

// Cycle/152 AC-4 — EmptyState renders title/hint/action.

describe("EmptyState (AC-4)", () => {
  it("renders the title and optional hint", () => {
    render(<EmptyState title="No memories yet" hint="Ingest via MCP" />);
    expect(screen.getByText("No memories yet")).toBeInTheDocument();
    expect(screen.getByText("Ingest via MCP")).toBeInTheDocument();
  });

  it("renders an action node when provided", () => {
    render(<EmptyState title="Empty" action={<button type="button">Add</button>} />);
    expect(screen.getByRole("button", { name: "Add" })).toBeInTheDocument();
  });

  it("does not render a Retry affordance (distinct from error)", () => {
    render(<EmptyState title="Empty" />);
    expect(screen.queryByRole("button", { name: /Retry/i })).not.toBeInTheDocument();
  });
});
