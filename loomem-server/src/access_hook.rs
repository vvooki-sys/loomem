//! /150e-2: server-side bridge from request context to the access-audit core
//! (ADR-018). Builds an `AccessRecord` from the request's `AuthContext` and
//! records it via `loomem_core::access_audit`.
//!
//! Layering (§4): this lives in the server layer because it touches
//! `AuthContext`; it constructs the domain `AccessRecord` and hands it to the
//! core. The core never sees HTTP types.

use loomem_core::access_audit::{self, AccessOp, AccessRecord};

use crate::auth::AuthContext;
use crate::AppState;

/// Record one data-plane access. **Gated** on `config.access_audit.enabled` —
/// a no-op when disabled, so behavior is byte-identical to pre-/150e (ADR-018
/// AC7). **Best-effort** (Q5): the core counts + warns on write failure; this
/// helper ignores the result so the read/search/store hot path is never blocked
/// or made to fail.
pub fn record_access(
    state: &AppState,
    auth: &AuthContext,
    op: AccessOp,
    target_id: Option<&str>,
    result_count: usize,
) {
    // Gate FIRST: when disabled this returns before any allocation, so the hot
    // path is byte-identical to pre-/150e (AC7). `target_id` is borrowed (not
    // cloned) at the call site for the same reason.
    if !state.config.access_audit.enabled {
        return;
    }
    let rec = AccessRecord {
        actor_user_id: auth.user_id.clone(),
        stream: auth.stream_id.clone(),
        role: format!("{:?}", auth.role),
        scope: format!("{:?}", auth.scope),
        op,
        target_id: target_id.map(str::to_string),
        result_count,
        ts: access_audit::now_unix(),
    };
    // Best-effort: failure is counted + warned inside `record`; never propagate.
    let _ = access_audit::record(&state.store, &rec);
}
