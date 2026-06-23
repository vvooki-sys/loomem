//! Context packing endpoint — smart context window manager.
//!
//! Packs relevant memories into a token-budgeted context block
//! ready for system prompt injection.

use axum::{extract::State, Json};
use std::sync::Arc;

use super::types::{ContextPackRequest, ContextPackResponse, ContextSource};
use super::AppError;
use crate::auth::{self, AuthContext};
use crate::AppState;

/// Estimate token count from text (simple heuristic: words * 1.3)
fn estimate_tokens(text: &str) -> usize {
    let word_count = text.split_whitespace().count();
    (word_count as f64 * 1.3) as usize
}

/// Truncate text to fit within a token budget.
fn truncate_to_budget(text: &str, budget_tokens: usize) -> (String, bool) {
    let words: Vec<&str> = text.split_whitespace().collect();
    let max_words = (budget_tokens as f64 / 1.3) as usize;

    if words.len() <= max_words {
        (text.to_string(), false)
    } else {
        let truncated = words[..max_words].join(" ");
        (format!("{}...", truncated), true)
    }
}

pub async fn context_pack_handler(
    State(state): State<Arc<AppState>>,
    axum::Extension(auth): axum::Extension<AuthContext>,
    Json(payload): Json<ContextPackRequest>,
) -> Result<Json<ContextPackResponse>, AppError> {
    // Scope the requested stream to what the caller is actually allowed to see.
    // Admins may request any stream; non-admins are pinned to their own.
    let stream = auth::validate_stream(&auth, payload.stream.as_deref())
        .map_err(|_| AppError::Forbidden("Cross-tenant stream access denied".into()))?;
    let budget = payload.budget_tokens.unwrap_or(4000);
    let _format = payload.format.as_deref().unwrap_or("markdown");

    let sections = payload.sections.unwrap_or_else(|| {
        vec![
            "profile".to_string(),
            "relevant".to_string(),
            "recent".to_string(),
        ]
    });

    // Budget allocation per section
    let section_count = sections.len().max(1);
    let profile_budget = if sections.contains(&"profile".to_string()) {
        budget * 20 / 100 // 20%
    } else {
        0
    };
    let relevant_budget = if sections.contains(&"relevant".to_string()) {
        budget * 50 / 100 // 50%
    } else {
        0
    };
    let remaining_budget = budget
        .saturating_sub(profile_budget)
        .saturating_sub(relevant_budget);
    let other_budget = remaining_budget;

    let mut context_parts: Vec<String> = Vec::new();
    let mut sources: Vec<ContextSource> = Vec::new();
    let mut sections_included: Vec<String> = Vec::new();
    let mut sections_truncated: Vec<String> = Vec::new();
    let mut total_tokens: usize = 0;

    // Build each section
    for section in &sections {
        if total_tokens >= budget {
            break;
        }

        let section_budget = match section.as_str() {
            "profile" => profile_budget,
            "relevant" => relevant_budget,
            _ => other_budget / (section_count.saturating_sub(2)).max(1),
        };

        if section_budget == 0 {
            continue;
        }

        match section.as_str() {
            "profile" => {
                // ADR-014 / cycle/139: stream-kind-aware. Private → UserProfile
                // (unchanged); shared/project → StreamManifest. Same routing
                // helper as memory_profile, so the section never injects a
                // person profile for a shared knowledge base.
                match crate::manifest::build_profile_or_manifest(&state, &stream, false).await {
                    Ok(result) => {
                        let md = result.to_markdown();
                        let (text, truncated) = truncate_to_budget(&md, section_budget);
                        total_tokens += estimate_tokens(&text);
                        context_parts.push(text);
                        sections_included.push("profile".to_string());
                        if truncated {
                            sections_truncated.push("profile".to_string());
                        }
                    }
                    Err(e) => {
                        tracing::warn!("Profile generation failed for context-pack: {}", e);
                    }
                }
            }

            "relevant" => {
                if let Some(ref query) = payload.query {
                    // BM25 search scoped to the caller's stream. Using the
                    // unfiltered `search()` here leaks chunks from other
                    // streams into the packed context.
                    let tantivy = state.tantivy.lock().await;
                    let results = tantivy
                        .search_with_stream(query, &stream, 10)
                        .unwrap_or_default();
                    drop(tantivy);

                    let mut section_text = String::from("## Relevant Memories\n\n");
                    let mut section_tokens = 0;

                    for r in &results {
                        // Prefix with date from the chunk for temporal reasoning
                        let date_prefix = {
                            let chunk = state.store.get_chunk(&r.id).ok().flatten();
                            let event_date = chunk
                                .as_ref()
                                .and_then(|c| c.extraction_meta.as_ref())
                                .and_then(|m| m.event_date.as_ref())
                                .cloned();
                            event_date.unwrap_or_else(|| {
                                chrono::DateTime::from_timestamp(r.timestamp, 0)
                                    .map(|d| d.format("%Y-%m-%d").to_string())
                                    .unwrap_or_default()
                            })
                        };
                        let line = if date_prefix.is_empty() {
                            format!("- {}\n", r.content)
                        } else {
                            format!("- [{}] {}\n", date_prefix, r.content)
                        };
                        let line_tokens = estimate_tokens(&line);
                        if section_tokens + line_tokens > section_budget {
                            sections_truncated.push("relevant".to_string());
                            break;
                        }
                        sources.push(ContextSource {
                            chunk_id: r.id.clone(),
                            section: "relevant".to_string(),
                            score: r.score as f64,
                        });
                        section_text.push_str(&line);
                        section_tokens += line_tokens;
                    }

                    total_tokens += section_tokens;
                    context_parts.push(section_text);
                    sections_included.push("relevant".to_string());
                }
            }

            "recent" | "recent_decisions" => {
                // Get recent chunks from this stream
                let mut recent_chunks: Vec<loomem_core::storage::Chunk> = Vec::new();

                for level in 0..=1 {
                    let prefix = format!("chunk:L{}:", level);
                    for (_key, value) in state.store.prefix_scan(prefix.as_bytes()) {
                        if let Ok(chunk) = state.store.decode_chunk(&value) {
                            // SEC-memctx-stream-leak: tombstoned chunks
                            // (`deleted_at.is_some()`) must be excluded — same
                            // gate as `tool_status` (dispatcher.rs). Without this check
                            // Recent Context resurrects soft-deleted chunks
                            // that no other read path surfaces.
                            if chunk.stream == stream
                                && chunk.is_latest
                                && !chunk.dormant
                                && chunk.deleted_at.is_none()
                            {
                                recent_chunks.push(chunk);
                            }
                        }
                    }
                }

                // Sort by timestamp desc, take recent
                recent_chunks.sort_by_key(|b| std::cmp::Reverse(b.timestamp));
                recent_chunks.truncate(15);

                let header = if section.as_str() == "recent_decisions" {
                    "## Recent Decisions\n\n"
                } else {
                    "## Recent Context\n\n"
                };
                let mut section_text = String::from(header);
                let mut section_tokens = 0;

                for chunk in &recent_chunks {
                    let date_prefix = chunk
                        .extraction_meta
                        .as_ref()
                        .and_then(|m| m.event_date.as_ref())
                        .cloned()
                        .unwrap_or_else(|| {
                            chrono::DateTime::from_timestamp(chunk.timestamp as i64, 0)
                                .map(|d| d.format("%Y-%m-%d").to_string())
                                .unwrap_or_default()
                        });
                    let line = if date_prefix.is_empty() {
                        format!("- {}\n", chunk.content)
                    } else {
                        format!("- [{}] {}\n", date_prefix, chunk.content)
                    };
                    let line_tokens = estimate_tokens(&line);
                    if section_tokens + line_tokens > section_budget {
                        sections_truncated.push(section.clone());
                        break;
                    }
                    sources.push(ContextSource {
                        chunk_id: chunk.id.clone(),
                        section: section.clone(),
                        score: chunk.importance.unwrap_or(1.0),
                    });
                    section_text.push_str(&line);
                    section_tokens += line_tokens;
                }

                total_tokens += section_tokens;
                context_parts.push(section_text);
                sections_included.push(section.clone());
            }

            _ => {
                tracing::warn!("Unknown context-pack section: {}", section);
            }
        }
    }

    let context = context_parts.join("\n");
    let final_tokens = estimate_tokens(&context);

    // Coverage score: how well we served the request
    let coverage = if sections.is_empty() {
        0.0
    } else {
        let included_ratio = sections_included.len() as f64 / sections.len() as f64;
        let truncated_penalty = sections_truncated.len() as f64 * 0.1;
        (included_ratio - truncated_penalty).clamp(0.0, 1.0)
    };

    Ok(Json(ContextPackResponse {
        context,
        token_count: final_tokens,
        sections_included,
        sections_truncated,
        sources,
        coverage_score: coverage,
    }))
}

#[cfg(test)]
mod tests {
    //! Cycle SEC-memctx-stream-leak regression tests.
    //!
    //! Mariusz observed `memory_context(stream=__shared_team__)` returning
    //! Recent Context entries tagged with another user's private stream
    //! (`team-private-stream`) on 2026-04-24 — a cross-stream leak.
    //!
    //! These tests exercise `context_pack_handler` directly (the same code
    //! path used by the MCP `memory_context` tool — see
    //! `loomem-server/src/mcp/dispatcher.rs::tool_context`) with two streams
    //! populated in a single tempdir-backed store and assert that:
    //!   1. Recent Context never returns a chunk whose `stream` differs from
    //!      the validated request stream.
    //!   2. Every chunk_id Recent Context returns for stream A is one of the
    //!      chunks the same store would surface to a `memory_search`-shape
    //!      caller on stream A.
    //!
    //! Per brief stop-and-ask trigger 4: both tests must fail on the pre-fix
    //! commit; if they pass before the fix the hypothesis is invalidated.
    use super::*;
    use crate::auth::{AuthContext, KeyScope};
    use loomem_core::storage::{Chunk, UserRole};
    use std::collections::HashSet;

    fn make_chunk(id: &str, stream: &str, content: &str, ts: u64) -> Chunk {
        Chunk {
            id: id.to_string(),
            content: content.to_string(),
            stream: stream.to_string(),
            level: 0,
            score: 1.0,
            timestamp: ts,
            consolidated: false,
            dormant: false,
            in_progress: false,
            prompt_version: None,
            source_ids: None,
            last_decay: None,
            metadata: None,
            importance: Some(1.0),
            persistent: false,
            last_implicit_boost: None,
            access_count: 0,
            source: None,
            created_by: None,
            updated_at: None,
            valid_from: None,
            valid_until: None,
            is_latest: true,
            superseded_by: None,
            supersedes_id: None,
            root_memory_id: None,
            version: 1,
            memory_type: None,
            extraction_meta: None,
            deleted_at: None,
            trust_level: None,
            ingester_user_id: None,
            alpha: 1.0,
            beta: 1.0,
            harmful_count: 0,
            n_ratings: 0,
            last_rated_at: None,
            provenance_role: loomem_core::storage::ProvenanceRole::Claim,
        }
    }

    fn admin_auth_for(stream_id: &str) -> AuthContext {
        AuthContext::single_stream(
            stream_id.to_string(),
            UserRole::Admin,
            KeyScope::Shared,
            None,
            true,
        )
    }

    /// Seed `state.store` with chunks in both streams and return the chunk ids
    /// per stream so the test can compare retrievals against ground truth.
    fn seed_two_streams(state: &Arc<AppState>) -> (Vec<String>, Vec<String>) {
        let mut a_ids = Vec::new();
        let mut b_ids = Vec::new();
        for i in 0u32..5 {
            let id = format!("a_{i}");
            state
                .store
                .store_chunk(&make_chunk(
                    &id,
                    "stream_A",
                    &format!("alpha content {i}"),
                    1_700_000_000 + u64::from(i),
                ))
                .expect("store stream_A chunk");
            a_ids.push(id);

            let id = format!("b_{i}");
            state
                .store
                .store_chunk(&make_chunk(
                    &id,
                    "stream_B",
                    &format!("beta content {i}"),
                    1_700_000_000 + u64::from(i),
                ))
                .expect("store stream_B chunk");
            b_ids.push(id);
        }
        (a_ids, b_ids)
    }

    /// Seed one tombstoned chunk (chunk.stream=stream_A, deleted_at=Some(...))
    /// with the rest of its state matching an active chunk (is_latest=true,
    /// dormant=false). Returns the chunk id so the test can assert it is
    /// absent from Recent Context output.
    fn seed_tombstone(state: &Arc<AppState>, id: &str) -> String {
        let mut chunk = make_chunk(
            id,
            "stream_A",
            "soft-deleted content that must NOT appear in Recent Context",
            1_700_000_999,
        );
        chunk.deleted_at = Some(1_700_000_500);
        state
            .store
            .store_chunk(&chunk)
            .expect("store tombstoned stream_A chunk");
        id.to_string()
    }

    /// Hard isolation: every Recent Context source for stream A must reference
    /// a chunk whose stream is "stream_A". Any other-stream chunk_id in
    /// `sources[section=recent]` proves a cross-stream leak.
    #[tokio::test]
    async fn memory_context_recent_does_not_leak_across_streams() {
        let (_app, state) = crate::tests::make_test_app();
        let (a_ids, b_ids) = seed_two_streams(&state);
        let b_set: HashSet<String> = b_ids.iter().cloned().collect();
        let a_set: HashSet<String> = a_ids.iter().cloned().collect();

        let req = ContextPackRequest {
            query: None,
            stream: Some("stream_A".into()),
            budget_tokens: Some(4000),
            sections: Some(vec!["recent".into()]),
            format: Some("markdown".into()),
        };
        let auth = admin_auth_for("stream_A");

        let resp = context_pack_handler(State(state.clone()), axum::Extension(auth), Json(req))
            .await
            .expect("context_pack_handler ok")
            .0;

        let recent_ids: Vec<&str> = resp
            .sources
            .iter()
            .filter(|s| s.section == "recent")
            .map(|s| s.chunk_id.as_str())
            .collect();

        assert!(
            !recent_ids.is_empty(),
            "Recent Context returned no sources — test setup error (expected stream_A chunks). \
             resp.sources = {:?}",
            resp.sources
        );

        for cid in &recent_ids {
            assert!(
                !b_set.contains(*cid),
                "CROSS-STREAM LEAK: Recent Context for stream_A returned chunk_id {cid} \
                 which belongs to stream_B. Full recent_ids = {recent_ids:?}"
            );
            assert!(
                a_set.contains(*cid),
                "Recent Context returned chunk_id {cid} that is not in stream_A's seeded set. \
                 a_ids = {a_ids:?}, b_ids = {b_ids:?}"
            );
        }
    }

    /// SEC-memctx-stream-leak §11/§12: tombstoned chunks (deleted_at.is_some())
    /// must NOT appear in Recent Context. Live probe on production showed
    /// `context.rs:186` was the only read path missing the `deleted_at.is_none()`
    /// gate, causing soft-deleted chunks to resurrect through `memory_context`
    /// while `memory_status`, `dashboard`, and `memory_search` correctly
    /// excluded them. This test pins that invariant.
    #[tokio::test]
    async fn memory_context_recent_excludes_tombstoned_chunks() {
        let (_app, state) = crate::tests::make_test_app();
        let (_a_ids, _b_ids) = seed_two_streams(&state);
        let tombstone_id = seed_tombstone(&state, "a_tombstone");

        let req = ContextPackRequest {
            query: None,
            stream: Some("stream_A".into()),
            budget_tokens: Some(4000),
            sections: Some(vec!["recent".into()]),
            format: Some("markdown".into()),
        };
        let auth = admin_auth_for("stream_A");

        let resp = context_pack_handler(State(state.clone()), axum::Extension(auth), Json(req))
            .await
            .expect("context_pack_handler ok")
            .0;

        let recent_ids: Vec<&str> = resp
            .sources
            .iter()
            .filter(|s| s.section == "recent")
            .map(|s| s.chunk_id.as_str())
            .collect();

        assert!(
            !recent_ids.contains(&tombstone_id.as_str()),
            "TOMBSTONE LEAK: Recent Context for stream_A returned the \
             soft-deleted chunk_id {tombstone_id}. recent_ids = {recent_ids:?}"
        );
        assert!(
            !resp
                .context
                .contains("soft-deleted content that must NOT appear"),
            "TOMBSTONE LEAK: rendered context text contains soft-deleted \
             content body. context = {ctx:?}",
            ctx = resp.context
        );
    }

    /// Parity with memory_search: every Recent Context chunk_id for stream A
    /// must also appear in the set of chunks the store would expose to a
    /// caller scanning stream A's chunks directly. Same invariant as test 1
    /// expressed via the search-shape ground truth (mirrors brief §Część 3
    /// scenario 2).
    #[tokio::test]
    async fn memory_context_recent_is_subset_of_search_for_same_stream() {
        let (_app, state) = crate::tests::make_test_app();
        let (a_ids, _b_ids) = seed_two_streams(&state);
        let search_set: HashSet<String> = a_ids.into_iter().collect();

        let req = ContextPackRequest {
            query: None,
            stream: Some("stream_A".into()),
            budget_tokens: Some(4000),
            sections: Some(vec!["recent".into()]),
            format: Some("markdown".into()),
        };
        let auth = admin_auth_for("stream_A");

        let resp = context_pack_handler(State(state.clone()), axum::Extension(auth), Json(req))
            .await
            .expect("context_pack_handler ok")
            .0;

        for src in resp.sources.iter().filter(|s| s.section == "recent") {
            assert!(
                search_set.contains(&src.chunk_id),
                "PARITY VIOLATION: Recent Context returned chunk_id {} which is NOT in the \
                 set of stream_A chunks (the same set memory_search would surface for \
                 stream_A). search_set = {:?}",
                src.chunk_id,
                search_set
            );
        }
    }

    /// AC-4 (cycle/139, ADR-014): the `profile` section of `memory_context` on
    /// a shared stream renders a knowledge-base manifest, NOT a person profile.
    /// The `recent` section is unaffected. Manifest LLM is disabled in the test
    /// config → no HTTP; routing is the SUT.
    #[tokio::test]
    async fn memory_context_profile_section_is_manifest_for_shared_stream() {
        let (_app, state) = crate::tests::make_test_app();
        let stream = "__shared_test139_ac4";
        state
            .store
            .store_chunk(&make_chunk(
                "s1",
                stream,
                "Anna migrated the search pipeline.",
                1_700_000_001,
            ))
            .expect("store shared chunk 1");
        state
            .store
            .store_chunk(&make_chunk(
                "s2",
                stream,
                "Bartek shipped billing.",
                1_700_000_002,
            ))
            .expect("store shared chunk 2");

        let req = ContextPackRequest {
            query: None,
            stream: Some(stream.into()),
            budget_tokens: Some(4000),
            sections: Some(vec!["profile".into(), "recent".into()]),
            format: Some("markdown".into()),
        };
        let auth = admin_auth_for(stream);

        let resp = context_pack_handler(State(state.clone()), axum::Extension(auth), Json(req))
            .await
            .expect("context_pack_handler ok")
            .0;

        assert!(
            resp.sections_included.contains(&"profile".to_string()),
            "profile section missing: {:?}",
            resp.sections_included
        );
        // The profile section is a manifest, not a person profile.
        assert!(
            resp.context.contains("# Knowledge Base"),
            "shared-stream profile section is not a manifest. context = {ctx:?}",
            ctx = resp.context
        );
        assert!(
            !resp.context.contains("### Identity"),
            "shared-stream context leaked a person identity. context = {ctx:?}",
            ctx = resp.context
        );
    }
}
