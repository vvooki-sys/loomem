// Typed error contract shared by every dashboard API layer.
//
// ApiError unifies fetch failures: callers branch on `status` (0 =
// network/parse failure) and read a human-readable `message`. The `code`,
// `error` and `body` fields are kept as aliases for consumers that prefer
// those names.
//
// Lives in its own module — NOT lib/api.js — so future api/* modules can
// import it without forming an import cycle.

export class ApiError extends Error {
  /**
   * @param {number} status HTTP status; 0 means network / parse failure.
   * @param {string} message Human-readable description.
   * @param {{ bodyText?: string, body?: unknown, error?: string }} [extra]
   */
  constructor(status, message, { bodyText, body, error } = {}) {
    super(message || `API ${status}`);
    this.name = "ApiError";
    this.status = status;
    // Alias — some consumers branch on `err.code`.
    this.code = status;
    this.bodyText = bodyText;
    this.body = body;
    this.error = error;
  }
}

// A 401 means the token is invalid or was rotated — the only full-page
// error. Drop the stored token and notify AuthGate (via a window event) to
// fall back to the login screen. Using a CustomEvent lets any fetch layer
// trigger the logout without importing React.
export function notifyUnauthorized() {
  try {
    localStorage.removeItem("loomem_api_key");
  } catch {
    /* ignore storage errors (private browsing etc.) */
  }
  window.dispatchEvent(new CustomEvent("loomem:unauthorized"));
}

// Short human text for a caught error, for toasts and inline error panels.
// Appends the HTTP status in parentheses unless it's a network/parse
// failure (status 0), which has no meaningful status code.
export function apiErrorText(err) {
  if (!err) return "Something went wrong.";
  const status = typeof err.status === "number" ? err.status : err.code;
  const base = err.message || "Request failed";
  if (!status) return base;
  return `${base} (${status})`;
}
