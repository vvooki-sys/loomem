import { renderHook, act } from "@testing-library/react";
import { afterEach, expect, it, vi } from "vitest";
import useMediaQuery from "../useMediaQuery";

afterEach(() => vi.restoreAllMocks());

it("tracks changes and unsubscribes when the media query changes", () => {
  let listener;
  let wide = false;
  const remove = vi.fn();
  vi.spyOn(window, "matchMedia").mockImplementation((query) => ({
    matches: query === "wide" && wide,
    addEventListener: (_type, callback) => { listener = callback; },
    removeEventListener: remove,
  }));
  const { result, rerender, unmount } = renderHook(({ query }) => useMediaQuery(query), {
    initialProps: { query: "wide" },
  });
  expect(result.current).toBe(false);
  act(() => { wide = true; listener(); });
  expect(result.current).toBe(true);
  rerender({ query: "narrow" });
  expect(result.current).toBe(false);
  expect(remove).toHaveBeenCalled();
  unmount();
  expect(remove).toHaveBeenCalledTimes(2);
});
