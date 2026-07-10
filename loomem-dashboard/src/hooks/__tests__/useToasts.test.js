import { describe, it, expect } from "vitest";
import { renderHook, act } from "@testing-library/react";
import useToasts from "../useToasts";

// Cycle/163 S7 / AC-11 — two identical messages within 3s collapse to one.

describe("useToasts dedupe (AC-11)", () => {
  it("suppresses a duplicate message pushed within the window", () => {
    const { result } = renderHook(() => useToasts());

    act(() => {
      result.current.push({ tone: "error", message: "Failed to load scopes" });
      result.current.push({ tone: "error", message: "Failed to load scopes" });
    });

    expect(result.current.toasts).toHaveLength(1);
  });

  it("allows distinct messages through", () => {
    const { result } = renderHook(() => useToasts());

    act(() => {
      result.current.push({ tone: "error", message: "A" });
      result.current.push({ tone: "error", message: "B" });
    });

    expect(result.current.toasts).toHaveLength(2);
  });
});
