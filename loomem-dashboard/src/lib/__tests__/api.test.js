import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import * as api from "../api";
import { ApiError } from "../apiError";

// fetchJSON throws a typed ApiError with the right status for every failure
// class, and a 401 triggers the global logout signal.

function res({ ok, status, statusText = "", body = "" }) {
  return {
    ok,
    status,
    statusText,
    text: () => Promise.resolve(typeof body === "string" ? body : JSON.stringify(body)),
    json: () => Promise.resolve(typeof body === "string" ? JSON.parse(body || "null") : body),
  };
}

beforeEach(() => {
  // Functional in-memory localStorage so the 401 test can assert the key is
  // cleared.
  const store = new Map();
  vi.stubGlobal("localStorage", {
    getItem: (k) => (store.has(k) ? store.get(k) : null),
    setItem: (k, v) => store.set(k, String(v)),
    removeItem: (k) => store.delete(k),
    clear: () => store.clear(),
  });
  global.fetch = vi.fn();
});

afterEach(() => {
  vi.restoreAllMocks();
});

describe("ApiError contract", () => {
  it.each([
    [403, "Forbidden"],
    [404, "Not Found"],
    [500, "Internal Server Error"],
  ])("throws ApiError with status %i", async (status, statusText) => {
    global.fetch.mockResolvedValueOnce(res({ ok: false, status, statusText }));
    const err = await api.getHealth().catch((e) => e);
    expect(err).toBeInstanceOf(ApiError);
    expect(err).toMatchObject({ status, code: status });
  });

  it("maps a network failure to status 0", async () => {
    global.fetch.mockRejectedValueOnce(new TypeError("Failed to fetch"));
    const err = await api.getHealth().catch((e) => e);
    expect(err).toBeInstanceOf(ApiError);
    expect(err.status).toBe(0);
  });

  it("prefers the JSON body.error as the message", async () => {
    global.fetch.mockResolvedValueOnce(
      res({ ok: false, status: 500, body: { error: "boom" } }),
    );
    const err = await api.getMemory().catch((e) => e);
    expect(err.message).toBe("boom");
    expect(err.status).toBe(500);
  });

  it("getMemoryChain throws instead of swallowing the error", async () => {
    global.fetch.mockResolvedValueOnce(res({ ok: false, status: 500 }));
    await expect(api.getMemoryChain("abc")).rejects.toBeInstanceOf(ApiError);
  });

  it("resolves parsed JSON on success", async () => {
    global.fetch.mockResolvedValueOnce(res({ ok: true, status: 200, body: { status: "healthy" } }));
    await expect(api.getHealth()).resolves.toEqual({ status: "healthy" });
  });

  it("attaches the stored token as a Bearer header", async () => {
    localStorage.setItem("loomem_api_key", "tok-123");
    global.fetch.mockResolvedValueOnce(res({ ok: true, status: 200, body: {} }));
    await api.getHealth();
    const [, options] = global.fetch.mock.calls[0];
    expect(options.headers.Authorization).toBe("Bearer tok-123");
  });
});

describe("401 → global logout", () => {
  it("clears the stored key and dispatches loomem:unauthorized", async () => {
    localStorage.setItem("loomem_api_key", "secret");
    const onEvent = vi.fn();
    window.addEventListener("loomem:unauthorized", onEvent);
    global.fetch.mockResolvedValueOnce(res({ ok: false, status: 401, statusText: "Unauthorized" }));

    const err = await api.getMemory().catch((e) => e);

    expect(err).toBeInstanceOf(ApiError);
    expect(err.status).toBe(401);
    expect(localStorage.getItem("loomem_api_key")).toBeNull();
    expect(onEvent).toHaveBeenCalledTimes(1);
    window.removeEventListener("loomem:unauthorized", onEvent);
  });
});
