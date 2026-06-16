//! `POST /v1/ambient` — Layer 1 ambient memory endpoint.
//!
//! Stage 3 of `cycle/103a-mvp-layer1-endpoint`. Wires the loomem-core
//! `ambient` module into a single HTTP handler:
//!
//! ```text
//! Request → auth + scope check → cache lookup (60s TTL)
//!                                    │
//!                          miss ─────┴───── hit → respond
//!                                    │
//!                          retrieve (BM25 + vector) → fuse w/ Layer 1 weights
//!                                    │
//!                          score per chunk (§10.5.1) → derive tier (§10.5.2)
//!                                    │
//!                          synthesize plain-fact text (§10.5.3 + B-2 fix)
//!                                    │
//!                          marker decision (§10.6.2 suppress cases)
//!                                    │
//!                          token-budget truncate (AC-3)
//!                                    │
//!                          cache + respond
//! ```
//!
//! AC mapping (per `cycles/cycle-103a-layer1-endpoint-brief.md`):
//!
//! | AC | What this handler enforces |
//! |----|---------------------------|
//! | AC-1 | Endpoint shape (`AmbientRequest` / `AmbientResponse`) |
//! | AC-2 | 100ms p50 timeout via `tokio::time::timeout`; `timeout_partial` reason |
//! | AC-3 | Token budget governance via `truncate_to_budget` |
//! | AC-4 | `{text, tier, score}` per-snippet schema |
//! | AC-5 | Marker construction via `decide_marker`, suppress per §10.6.2 |
//! | AC-6 | Scope inheritance via `validate_scope_access` |
//! | AC-7 | Recency-tuned retrieval via `apply_layer1_fusion` (τ=7d, w=0.15) |
//! | AC-8 | Multi-hop disabled (graph_edge weight = 0; no graph traversal) |
//! | AC-9 | In-memory LRU cache, 60s TTL |
//! | AC-10 | TTL-only invalidation (event bus deferred to /103a-full) |
//! | AC-11 | `debug` field server-side-only (B-1 fix) |
//! | AC-12 | Graceful degradation: HTTP 200 + `degraded_retrieval` reason |
//! | AC-13 | Plain-fact constraint enforced at synthesis time (fail-closed) |
//! | AC-15 | Type-uniform: NO `/85 classifier` import here |

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::{extract::State, Json};

use loomem_core::ambient::{
    apply_layer1_fusion, build_key, build_marker, compute_agreement, compute_confidence,
    decide_marker, derive_tier, provenance_class_for_level, recency_from_breakdown,
    synthesize_snippet, truncate_to_budget, AmbientDebug, AmbientLatencyMs, AmbientRequest,
    AmbientResponse, AmbientSnippet, MarkerOutcome, NegativeReason, SuppressContext, Tier,
};
use loomem_core::HybridSearchResult;

use super::AppError;
use crate::auth::AuthContext;
use crate::AppState;

/// AC-2 latency budget on the synthesis hot path. Cache-hit / synthesis path
/// uses `tokio::time::timeout` with this value; on expiry the response carries
/// `marker_if_empty.reason = "timeout_partial"`.
const LATENCY_BUDGET: Duration = Duration::from_millis(100);

/// AC-7 narrow Layer 1 retrieval. Top-N=5 ambient snippets per §10.7 #7;
/// we retrieve a slightly larger candidate pool to give RRF something to
/// rank, then take the top 5 after fusion.
const TOP_K_RETRIEVAL: usize = 20;
const TOP_N_SNIPPETS: usize = 5;

/// AC-1 + AC-12 + AC-13: the single public handler.
pub async fn ambient_handler(
    State(state): State<Arc<AppState>>,
    axum::Extension(auth): axum::Extension<AuthContext>,
    Json(payload): Json<AmbientRequest>,
) -> Result<Json<AmbientResponse>, AppError> {
    let start = Instant::now();

    validate_scope_access(&auth, &payload.scope)?;

    // AC-9 cache fast path — skipped when caller asks for `refresh: true`.
    if !payload.refresh {
        let key = cache_key(&payload);
        if let Some(cached) = state.ambient_cache.get(&key) {
            return Ok(Json(cached));
        }
    }

    // AC-2 graceful degradation under timeout. The synthesis path is sync-
    // shaped per AC-2 design (deterministic templates, no LLM hop), but
    // storage / embedding can take milliseconds; we wrap to bound the worst
    // case.
    let body = match tokio::time::timeout(
        LATENCY_BUDGET,
        run_layer1_pipeline(&state, &auth, &payload, start),
    )
    .await
    {
        Ok(Ok(resp)) => resp,
        Ok(Err(err)) => degraded_response(
            &payload.scope,
            start,
            NegativeReason::DegradedRetrieval,
            err,
        ),
        Err(_) => degraded_response(
            &payload.scope,
            start,
            NegativeReason::TimeoutPartial,
            anyhow::anyhow!(
                "ambient pipeline exceeded {}ms latency budget",
                LATENCY_BUDGET.as_millis()
            ),
        ),
    };

    // Cache the served response (excluding `refresh: true` calls — those
    // explicitly asked for fresh, no point seeding cache from them).
    if !payload.refresh {
        let key = cache_key(&payload);
        state.ambient_cache.put(key, body.clone());
    }

    Ok(Json(body))
}

/// AC-6: caller MUST have access to the requested scope. Admins always pass;
/// non-admins must have a membership whose `stream_id` matches the trailing
/// `:<id>` portion of `scope` ("private:user_42" → match user_42), OR the
/// raw `scope` string itself if it's already a stream id.
fn validate_scope_access(auth: &AuthContext, scope: &str) -> Result<(), AppError> {
    if auth.is_admin {
        return Ok(());
    }
    let stream_id = scope.split(':').next_back().unwrap_or(scope);
    let has_membership =
        auth.memberships.iter().any(|m| m.stream_id == stream_id) || auth.stream_id == stream_id;
    if has_membership {
        Ok(())
    } else {
        Err(AppError::Forbidden(format!(
            "no membership for scope '{scope}'"
        )))
    }
}

/// Build a cache key per AC-9 — blake3 over (user_id, scope, recent_turns,
/// hint, stream). Suppress flag is part of the request envelope but not
/// part of the key (toggling it shouldn't recompute retrieval). `stream`
/// IS part of the key — different streams produce different retrievals
/// (cycle/103c isolation requirement).
fn cache_key(payload: &AmbientRequest) -> [u8; 32] {
    build_key(
        &payload.user_id,
        &payload.scope,
        payload.recent_turns.as_deref(),
        payload.hint.as_deref(),
        payload.stream.as_deref(),
    )
}

/// The Layer 1 synthesis pipeline. Returns the response body OR an error
/// the caller will translate into a degraded response per AC-12.
async fn run_layer1_pipeline(
    state: &Arc<AppState>,
    auth: &AuthContext,
    payload: &AmbientRequest,
    start: Instant,
) -> Result<AmbientResponse, anyhow::Error> {
    let query = derive_query_text(payload);
    let retrieval_start = Instant::now();
    let candidates = retrieve_candidates(state, auth, &query, payload.stream.as_deref()).await?;
    let retrieval_ms = retrieval_start.elapsed().as_millis() as u32;

    let synthesis_start = Instant::now();
    let now_ts = chrono::Utc::now().timestamp();
    let fusion = apply_layer1_fusion(&candidates, now_ts);
    let snippets = score_and_synthesize(&candidates, &fusion);
    let kept = truncate_to_budget(snippets, None)?;
    let synthesis_ms = synthesis_start.elapsed().as_millis() as u32;

    let marker_payload = if kept.is_empty() {
        let ctx = SuppressContext {
            explicit_suppress: payload.suppress_negative_marker,
            chunk_count_in_scope: candidates.len() as u64,
            turn_index: payload.recent_turns.as_ref().map_or(0, |t| t.len() as u32),
            scope_mismatch: false,
        };
        let reason = if candidates.is_empty() {
            NegativeReason::ZeroChunks
        } else {
            NegativeReason::AllLowTier
        };
        match decide_marker(&payload.scope, reason, ctx) {
            MarkerOutcome::Emit(m) => Some(m),
            MarkerOutcome::Suppressed => None,
        }
    } else {
        None
    };

    let trace_ids = candidates
        .iter()
        .take(TOP_N_SNIPPETS)
        .map(|c| c.id.clone())
        .collect();
    let breakdowns: Vec<_> = fusion
        .breakdowns
        .iter()
        .take(TOP_N_SNIPPETS)
        .cloned()
        .collect();

    let total_ms = start.elapsed().as_millis() as u32;
    Ok(AmbientResponse {
        snippets: kept,
        marker_if_empty: marker_payload,
        debug: AmbientDebug {
            trace_ids,
            signal_breakdown_per_snippet: breakdowns,
            latency_ms: AmbientLatencyMs {
                retrieval: retrieval_ms,
                synthesis: synthesis_ms,
                total: total_ms,
            },
            error: None,
        },
    })
}

/// Build the retrieval query string from request inputs. Priority: explicit
/// `hint` → last user `recent_turn` content → empty (drives ZeroChunks).
fn derive_query_text(payload: &AmbientRequest) -> String {
    if let Some(hint) = payload.hint.as_deref() {
        if !hint.is_empty() {
            return hint.to_string();
        }
    }
    if let Some(turns) = payload.recent_turns.as_ref() {
        for turn in turns.iter().rev() {
            if turn.role == "user" && !turn.content.is_empty() {
                return turn.content.clone();
            }
        }
    }
    String::new()
}

/// Retrieve up to `TOP_K_RETRIEVAL` candidates via BM25 + vector search,
/// fused by `HybridSearchEngine`. Two filter layers:
///
/// 1. **`stream` filter** (cycle/103c) — when `stream.is_some()`, restrict
///    Tantivy + vector retrieval to chunks whose `Chunk::stream` matches.
///    Mirrors the `/v1/search` `stream` filter pattern. Required for
///    multi-tenant isolation in instances with N per-question streams
///    (e.g. LongMemEval-S `lme_<qid>` setup).
/// 2. **`auth.memberships` filter** (existing) — defense-in-depth post-
///    retrieval check that no chunks leak past auth boundary.
async fn retrieve_candidates(
    state: &Arc<AppState>,
    auth: &AuthContext,
    query: &str,
    stream: Option<&str>,
) -> Result<Vec<HybridSearchResult>, anyhow::Error> {
    if query.is_empty() {
        return Ok(Vec::new());
    }
    // BM25 — stream-scoped when stream filter is set.
    let bm25 = {
        let tantivy = state.tantivy.lock().await;
        match stream {
            Some(s) => tantivy
                .search_with_stream(query, s, TOP_K_RETRIEVAL)
                .unwrap_or_default(),
            None => tantivy.search(query, TOP_K_RETRIEVAL).unwrap_or_default(),
        }
    };

    // Vector path: only available when an embedder is configured. MVP falls
    // back to BM25-only when the embedder isn't initialized (test fixtures,
    // minimal dev configs). When `stream` filter is set, pre-filter
    // embeddings by chunk's stream BEFORE vector_search — without this,
    // vector top-K picks from all streams then post-filter drops most.
    let vector_scores: Vec<(String, f32)> = if let Some(ref embedder) = state.local_embedder {
        match embedder.embed(query) {
            Ok(query_emb) => match state.store.get_all_embeddings() {
                Ok(all_embs) => {
                    let filtered = match stream {
                        Some(s) => all_embs
                            .into_iter()
                            .filter(|(id, _)| {
                                state
                                    .store
                                    .get_chunk(id)
                                    .ok()
                                    .flatten()
                                    .map(|c| c.stream == s)
                                    .unwrap_or(false)
                            })
                            .collect::<Vec<_>>(),
                        None => all_embs,
                    };
                    loomem_core::vector_search::vector_search(
                        &filtered,
                        &query_emb,
                        TOP_K_RETRIEVAL,
                    )
                }
                Err(_) => Vec::new(),
            },
            Err(_) => Vec::new(),
        }
    } else {
        Vec::new()
    };

    let fused = if vector_scores.is_empty() {
        state.hybrid_search.bm25_only(bm25)?
    } else {
        state
            .hybrid_search
            .fuse_with_vector(bm25, vector_scores, Some(&state.store))?
    };

    // Defense-in-depth: post-retrieval filter on chunk's stream when
    // requested. Both Tantivy `search_with_stream` and the embedding
    // pre-filter above should already enforce this; the redundant check
    // here guarantees no chunk from a different stream reaches the agent
    // even if a future code path bypasses one of the upstream filters.
    let stream_pass = |c: &HybridSearchResult| -> bool {
        match stream {
            None => true,
            Some(s) => state
                .store
                .get_chunk(&c.id)
                .ok()
                .flatten()
                .map(|chunk| chunk.stream == s)
                .unwrap_or(false),
        }
    };

    // Auth memberships filter (existing).
    let allowed: std::collections::HashSet<&str> = auth
        .memberships
        .iter()
        .map(|m| m.stream_id.as_str())
        .collect();
    let scoped: Vec<HybridSearchResult> = fused
        .into_iter()
        .filter(|c| auth.is_admin || allowed.contains(c.user_id.as_str()))
        .filter(stream_pass)
        .take(TOP_K_RETRIEVAL)
        .collect();
    Ok(scoped)
}

/// Per-candidate confidence + synthesis. Drops candidates that fail the
/// §10.5.3 plain-fact constraint (synthesis returns Err) or that exceed
/// the per-snippet token cap.
fn score_and_synthesize(
    candidates: &[HybridSearchResult],
    fusion: &loomem_core::search::fusion::FusionResult,
) -> Vec<AmbientSnippet> {
    // §10.5.1 expects `rrf_fused ∈ [0, 1]` (per memory-routing.md "Zakres
    // faktyczny RRF na produkcji: ~0.0–1.0 po normalizacji"). The raw
    // `search::fusion::fuse` output is the un-normalized RRF sum which
    // lives in [0, 1/(k+1)] ≈ [0, 0.0164] for k=60. Normalize by the
    // top-1 score within the result set so top-1 → 1.0 and the formula's
    // 0.50 weight on rrf_fused contributes meaningfully. Final τ_high /
    // τ_medium calibration is a /103a-pre item (§10.10 #6).
    let max_score = fusion.fused_scores.iter().copied().fold(0.0_f32, f32::max);
    let mut out = Vec::new();
    for &idx in fusion.fused_order.iter().take(TOP_N_SNIPPETS) {
        let Some(cand) = candidates.get(idx) else {
            continue;
        };
        let Some(breakdown) = fusion.breakdowns.get(idx) else {
            continue;
        };
        let raw_rrf = fusion.fused_scores.get(idx).copied().unwrap_or(0.0);
        let rrf_normalized = if max_score > 0.0 {
            (raw_rrf / max_score).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let provenance = provenance_class_for_level(cand.level);
        let recency = recency_from_breakdown(breakdown);
        let agreement = compute_agreement(candidates, idx);
        let c = compute_confidence(rrf_normalized, provenance, recency, agreement);
        let tier = derive_tier(c, false);
        if tier == Tier::Low {
            continue;
        }
        if let Ok(snippet) = synthesize_snippet(cand, c, tier) {
            out.push(snippet);
        }
    }
    out
}

/// AC-12 graceful degradation. Builds an HTTP-200 response carrying an
/// explicit failure marker so the agent can fall back to `memory_search`.
fn degraded_response(
    scope: &str,
    start: Instant,
    reason: NegativeReason,
    err: anyhow::Error,
) -> AmbientResponse {
    tracing::warn!(
        target: "ambient",
        ?reason,
        scope,
        error = %err,
        "ambient pipeline degraded — emitting marker"
    );
    let marker = build_marker(scope, reason);
    let total_ms = start.elapsed().as_millis() as u32;
    AmbientResponse {
        snippets: Vec::new(),
        marker_if_empty: Some(marker),
        debug: AmbientDebug {
            trace_ids: Vec::new(),
            signal_breakdown_per_snippet: Vec::new(),
            latency_ms: AmbientLatencyMs {
                retrieval: 0,
                synthesis: 0,
                total: total_ms,
            },
            error: Some(format!("{err:#}")),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{KeyScope, UserRole};
    use loomem_core::ambient::RecentTurn;

    fn admin_auth() -> AuthContext {
        AuthContext::single_stream(
            "__shared_team__",
            UserRole::Admin,
            KeyScope::Shared,
            Some("admin_user".to_string()),
            true,
        )
    }

    fn user_auth(user_id: &str) -> AuthContext {
        AuthContext::single_stream(
            user_id,
            UserRole::Admin,
            KeyScope::Private,
            Some(user_id.to_string()),
            false,
        )
    }

    #[test]
    fn validate_scope_admin_passes_anywhere() {
        let auth = admin_auth();
        assert!(validate_scope_access(&auth, "private:any_user").is_ok());
        assert!(validate_scope_access(&auth, "shared:any_stream").is_ok());
    }

    #[test]
    fn validate_scope_member_passes_for_own_stream() {
        let auth = user_auth("__user_42__");
        assert!(validate_scope_access(&auth, "private:__user_42__").is_ok());
        assert!(validate_scope_access(&auth, "__user_42__").is_ok());
    }

    #[test]
    fn validate_scope_non_member_rejected() {
        let auth = user_auth("__user_42__");
        let result = validate_scope_access(&auth, "private:__user_99__");
        assert!(matches!(result, Err(AppError::Forbidden(_))));
    }

    #[test]
    fn derive_query_prefers_hint_over_turns() {
        let req = AmbientRequest {
            user_id: "u".to_string(),
            scope: "s".to_string(),
            recent_turns: Some(vec![RecentTurn {
                role: "user".to_string(),
                content: "earlier turn".to_string(),
            }]),
            hint: Some("explicit hint".to_string()),
            refresh: false,
            suppress_negative_marker: false,
            stream: None,
        };
        assert_eq!(derive_query_text(&req), "explicit hint");
    }

    #[test]
    fn derive_query_falls_back_to_last_user_turn() {
        let req = AmbientRequest {
            user_id: "u".to_string(),
            scope: "s".to_string(),
            recent_turns: Some(vec![
                RecentTurn {
                    role: "user".to_string(),
                    content: "first".to_string(),
                },
                RecentTurn {
                    role: "assistant".to_string(),
                    content: "reply".to_string(),
                },
                RecentTurn {
                    role: "user".to_string(),
                    content: "last user turn".to_string(),
                },
            ]),
            hint: None,
            refresh: false,
            suppress_negative_marker: false,
            stream: None,
        };
        assert_eq!(derive_query_text(&req), "last user turn");
    }

    #[test]
    fn derive_query_empty_when_no_input() {
        let req = AmbientRequest {
            user_id: "u".to_string(),
            scope: "s".to_string(),
            recent_turns: None,
            hint: None,
            refresh: false,
            suppress_negative_marker: false,
            stream: None,
        };
        assert_eq!(derive_query_text(&req), "");
    }

    #[test]
    fn cache_key_stable_for_identical_payloads() {
        let req1 = AmbientRequest {
            user_id: "u".to_string(),
            scope: "private:u".to_string(),
            recent_turns: None,
            hint: Some("x".to_string()),
            refresh: false,
            suppress_negative_marker: false,
            stream: None,
        };
        let req2 = AmbientRequest {
            user_id: "u".to_string(),
            scope: "private:u".to_string(),
            recent_turns: None,
            hint: Some("x".to_string()),
            refresh: true,                  // refresh flag NOT part of key
            suppress_negative_marker: true, // also NOT part of key
            stream: None,
        };
        assert_eq!(cache_key(&req1), cache_key(&req2));
    }

    #[test]
    fn cache_key_differs_when_stream_differs() {
        // /103c isolation: same (user, scope, hint) with different `stream`
        // MUST produce different cache keys.
        let req_a = AmbientRequest {
            user_id: "u".to_string(),
            scope: "private:u".to_string(),
            recent_turns: None,
            hint: Some("x".to_string()),
            refresh: false,
            suppress_negative_marker: false,
            stream: Some("lme_q1".to_string()),
        };
        let req_b = AmbientRequest {
            user_id: "u".to_string(),
            scope: "private:u".to_string(),
            recent_turns: None,
            hint: Some("x".to_string()),
            refresh: false,
            suppress_negative_marker: false,
            stream: Some("lme_q2".to_string()),
        };
        assert_ne!(
            cache_key(&req_a),
            cache_key(&req_b),
            "different streams MUST produce different cache keys"
        );
    }

    #[test]
    fn degraded_response_carries_explicit_reason() {
        let resp = degraded_response(
            "private:u",
            Instant::now(),
            NegativeReason::DegradedRetrieval,
            anyhow::anyhow!("storage offline"),
        );
        assert!(resp.snippets.is_empty());
        let marker = resp.marker_if_empty.expect("marker must be present");
        assert_eq!(marker.reason, NegativeReason::DegradedRetrieval);
        assert!(resp.debug.error.is_some());
    }

    #[test]
    fn degraded_response_timeout_uses_timeout_reason() {
        let resp = degraded_response(
            "s",
            Instant::now(),
            NegativeReason::TimeoutPartial,
            anyhow::anyhow!("budget exceeded"),
        );
        let marker = resp.marker_if_empty.expect("marker present");
        assert_eq!(marker.reason, NegativeReason::TimeoutPartial);
    }

    #[tokio::test]
    async fn empty_query_returns_zero_chunks_marker_via_handler() {
        let (_router, state) = crate::tests::make_test_app();
        let auth = user_auth("__user_42__");
        // Provide enough recent_turns to escape cold-start grace window
        // (§10.6.2 #1 — first 5 turns of brand-new user are suppressed).
        let recent_turns = (0..6)
            .map(|i| RecentTurn {
                role: if i % 2 == 0 { "user" } else { "assistant" }.to_string(),
                content: String::new(), // empty content → empty derived query
            })
            .collect();
        let payload = AmbientRequest {
            user_id: "u_42".to_string(),
            scope: "__user_42__".to_string(),
            recent_turns: Some(recent_turns),
            hint: None, // empty query → no retrieval → ZeroChunks marker
            refresh: false,
            suppress_negative_marker: false,
            stream: None,
        };
        let resp = ambient_handler(State(state), axum::Extension(auth), Json(payload))
            .await
            .expect("handler ok");
        assert!(resp.0.snippets.is_empty());
        let marker = resp
            .0
            .marker_if_empty
            .expect("marker present on empty retrieval (post cold-start grace)");
        assert_eq!(marker.reason, NegativeReason::ZeroChunks);
    }

    #[tokio::test]
    async fn cold_start_suppresses_marker_via_handler() {
        // Inverse of the above: turn_index=0 with no chunks_in_scope → cold-start
        // grace path → `marker_if_empty` should be None (§10.6.2 #1).
        let (_router, state) = crate::tests::make_test_app();
        let auth = user_auth("__user_42__");
        let payload = AmbientRequest {
            user_id: "u_42".to_string(),
            scope: "__user_42__".to_string(),
            recent_turns: None, // turn_index = 0
            hint: None,
            refresh: false,
            suppress_negative_marker: false,
            stream: None,
        };
        let resp = ambient_handler(State(state), axum::Extension(auth), Json(payload))
            .await
            .expect("handler ok");
        assert!(resp.0.snippets.is_empty());
        assert!(
            resp.0.marker_if_empty.is_none(),
            "cold-start grace MUST suppress marker on first turn"
        );
    }

    #[tokio::test]
    async fn auth_rejected_when_no_membership() {
        let (_router, state) = crate::tests::make_test_app();
        let auth = user_auth("__user_42__");
        let payload = AmbientRequest {
            user_id: "u".to_string(),
            scope: "private:__user_99__".to_string(),
            recent_turns: None,
            hint: Some("anything".to_string()),
            refresh: false,
            suppress_negative_marker: false,
            stream: None,
        };
        let result = ambient_handler(State(state), axum::Extension(auth), Json(payload)).await;
        assert!(
            matches!(result, Err(AppError::Forbidden(_))),
            "non-member request to other user's scope must 403"
        );
    }
}
