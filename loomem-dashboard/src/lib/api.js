// ═══════════════════════════════════════════════════
// LOOMEM DASHBOARD — API Service Layer
// ═══════════════════════════════════════════════════
//
// Calls real loomem-server endpoints (same-origin in production, proxied via
// Vite to :3030 in dev). Every helper returns parsed data or throws a typed
// `ApiError`. There is no mock fallback: failures surface to the UI as error
// states so the dashboard never renders fabricated zeros in place of a real
// error.

import { ApiError, notifyUnauthorized } from './apiError';

export { ApiError } from './apiError';

const API_BASE = '';

// ── Helpers ──

async function fetchJSON(url, options = {}) {
  const headers = { 'Content-Type': 'application/json', ...options.headers };
  // Single-user token unlock: the API key lives in localStorage after login.
  const apiKey = localStorage.getItem('loomem_api_key');
  if (apiKey) headers['Authorization'] = `Bearer ${apiKey}`;

  let res;
  try {
    res = await fetch(`${API_BASE}${url}`, { ...options, headers });
  } catch (err) {
    // Network / DNS / abort failure — no HTTP status is available.
    throw new ApiError(0, err?.message || 'Network error');
  }

  if (!res.ok) {
    // 401 = token invalid/rotated → global logout (only full-page error).
    if (res.status === 401) notifyUnauthorized();
    let bodyText;
    let message = `API ${res.status}: ${res.statusText}`;
    try {
      bodyText = await res.text();
      if (bodyText) {
        try {
          const body = JSON.parse(bodyText);
          if (body?.error) message = body.error;
          else if (body?.message) message = body.message;
        } catch {
          // Not JSON — keep the raw bodyText for callers that want it.
        }
      }
    } catch {
      // Body already consumed / unreadable — keep the default message.
    }
    throw new ApiError(res.status, message, { bodyText });
  }

  // Some endpoints return an empty body; treat that as null rather than a
  // parse error.
  const text = await res.text();
  if (!text) return null;
  try {
    return JSON.parse(text);
  } catch {
    throw new ApiError(0, 'Invalid JSON response from server');
  }
}

// ═══════════════════════════════════════════════════
// API CALLS — loomem-server endpoints
// ═══════════════════════════════════════════════════

// Health check — GET /health (public, carries the server version)
export async function getHealth() {
  return fetchJSON('/health');
}

// Memory search — POST /v1/search (hybrid BM25 + vector)
export async function searchMemories(query, options = {}) {
  return fetchJSON('/v1/search', {
    method: 'POST',
    body: JSON.stringify({ query, top_k: options.top_k ?? 50, ...options }),
  });
}

// Engine status — GET /v1/status
export async function getMemoryStatus() {
  return fetchJSON('/v1/status');
}

// Memory list — GET /api/dashboard/memory?page=&per_page=&q=&layer=&source_agent=&entity_id=
// Single-user: the server reads the instance's default stream.
export async function getMemory(params = {}) {
  const qs = new URLSearchParams(params).toString();
  const url = qs ? `/api/dashboard/memory?${qs}` : '/api/dashboard/memory';
  return fetchJSON(url);
}

// GET /v1/memory-chain/:id — version history for a chunk. Throws on error —
// the History tab renders an ErrorState instead of an eternal spinner.
export async function getMemoryChain(id) {
  return fetchJSON(`/v1/memory-chain/${id}`);
}

// PUT /api/memories/:id — update memory content/confidence/category
export async function updateMemory(id, updates) {
  return fetchJSON(`/api/memories/${id}`, {
    method: 'PUT',
    body: JSON.stringify(updates),
  });
}

// DELETE /api/memories/:id — soft-delete a memory
export async function deleteMemory(id) {
  return fetchJSON(`/api/memories/${id}`, {
    method: 'DELETE',
  });
}

// Reality bench trendline — GET /v1/admin/bench/history. Returns a sorted
// array of {filename, kind, timestamp, hit_rate, total_questions}.
export async function getBenchHistory() {
  const data = await fetchJSON('/v1/admin/bench/history');
  return Array.isArray(data) ? data : [];
}
