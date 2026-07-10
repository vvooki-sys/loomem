import { describe, it, expect } from "vitest";
import { errorCopy } from "../errorCopy";
import { ApiError } from "../apiError";

// Cycle/163 S7 / AC-12 — the dictionary maps known conditions to human EN copy
// and never lets raw backend jargon through.

describe("errorCopy (AC-12)", () => {
  it("maps the no-user-identity backend string to friendly copy", () => {
    const err = new ApiError(400, "authentication has no user identity");
    expect(errorCopy(err)).toBe(
      "This session has no user identity. User-scoped views are unavailable.",
    );
    // The raw backend phrase must never survive into the returned copy.
    expect(errorCopy(err)).not.toMatch(/authentication has no user identity/i);
  });

  it("maps 403 to a permission message", () => {
    expect(errorCopy(new ApiError(403, "forbidden xyz"))).toBe(
      "You don't have permission to view this.",
    );
  });

  it("maps status 0 / network failure to a reach-the-server message", () => {
    expect(errorCopy(new ApiError(0, "Failed to fetch"))).toBe(
      "Can't reach the server. Check your connection and retry.",
    );
  });

  it("falls back to apiErrorText for unmapped errors (keeps the status)", () => {
    expect(errorCopy(new ApiError(500, "boom"))).toBe("boom (500)");
  });
});
