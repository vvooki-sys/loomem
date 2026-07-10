import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { render, screen, waitFor, act } from "@testing-library/react";
import AuthGate from "../AuthGate";

// A `loomem:unauthorized` event (dispatched by any fetch layer on a 401)
// drops an unlocked session back to the login screen.

beforeEach(() => {
  const store = new Map([["loomem_api_key", "k"]]);
  vi.stubGlobal("localStorage", {
    getItem: (k) => (store.has(k) ? store.get(k) : null),
    setItem: (k, v) => store.set(k, String(v)),
    removeItem: (k) => store.delete(k),
    clear: () => store.clear(),
  });
  global.fetch = vi.fn((url) => {
    if (url === "/v1/whoami")
      return Promise.resolve({
        ok: true,
        json: () =>
          Promise.resolve({ stream_id: "s1", is_admin: true, role: "Admin", user_id: null }),
      });
    return Promise.resolve({ ok: true, json: () => Promise.resolve({}) });
  });
});

afterEach(() => {
  vi.restoreAllMocks();
});

describe("AuthGate 401 flow", () => {
  it("returns to the login screen when loomem:unauthorized fires", async () => {
    render(<AuthGate>{() => <div>SECRET DASHBOARD</div>}</AuthGate>);

    // Unlocked → children render.
    await waitFor(() => expect(screen.getByText("SECRET DASHBOARD")).toBeInTheDocument());

    // Simulate a 401 anywhere in the app.
    await act(async () => {
      window.dispatchEvent(new CustomEvent("loomem:unauthorized"));
    });

    await waitFor(() => expect(screen.queryByText("SECRET DASHBOARD")).not.toBeInTheDocument());
    expect(screen.getByText(/session expired/i)).toBeInTheDocument();
    expect(screen.getByRole("button", { name: /Unlock/i })).toBeInTheDocument();
  });

  it("rejects a bad token with an inline error", async () => {
    global.fetch = vi.fn((url, opts) => {
      const auth = opts?.headers?.Authorization;
      if (url === "/v1/whoami" && auth === "Bearer good-token")
        return Promise.resolve({
          ok: true,
          json: () => Promise.resolve({ stream_id: "s1", is_admin: true, role: "Admin" }),
        });
      return Promise.resolve({ ok: false, status: 401, json: () => Promise.resolve({}) });
    });
    localStorage.removeItem("loomem_api_key");

    const user = (await import("@testing-library/user-event")).default;
    render(<AuthGate>{() => <div>SECRET DASHBOARD</div>}</AuthGate>);

    const input = await screen.findByPlaceholderText("API token");
    await user.type(input, "wrong-token");
    await user.click(screen.getByRole("button", { name: /Unlock/i }));
    await waitFor(() => expect(screen.getByText(/Invalid API token/i)).toBeInTheDocument());

    await user.clear(input);
    await user.type(input, "good-token");
    await user.click(screen.getByRole("button", { name: /Unlock/i }));
    await waitFor(() => expect(screen.getByText("SECRET DASHBOARD")).toBeInTheDocument());
    expect(localStorage.getItem("loomem_api_key")).toBe("good-token");
  });
});
