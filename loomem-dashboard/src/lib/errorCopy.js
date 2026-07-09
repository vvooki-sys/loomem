// Human-readable error copy dictionary (cycle/163 S7, live audit L-2).
//
// Maps known error conditions — backend jargon, HTTP status classes, network
// failures — onto plain EN copy. Unknown errors fall back to apiErrorText so we
// never drop signal. Raw backend strings (e.g. "authentication has no user
// identity") are matched HERE and must never reach the UI verbatim: both
// toasts and ErrorState route through this function so a single dictionary
// governs what a user actually reads.

import { apiErrorText } from "./apiError";

export function errorCopy(err) {
  if (!err) return "Something went wrong.";
  const status = typeof err.status === "number" ? err.status : err.code;
  const raw = (err.message || "").toLowerCase();

  // No user identity — the service-token / env-token session has no user_id,
  // so user-scoped views are meaningless. Backend surfaces this as
  // "authentication has no user identity"; never leak that phrasing.
  if (raw.includes("authentication has no user identity") || raw.includes("no user identity")) {
    return "This session has no user identity. User-scoped views are unavailable.";
  }
  if (status === 401) return "Your session has expired. Please sign in again.";
  if (status === 403) return "You don't have permission to view this.";
  // Status 0 = network / DNS / abort / parse failure (see lib/apiError.js).
  if (!status) return "Can't reach the server. Check your connection and retry.";
  return apiErrorText(err);
}
