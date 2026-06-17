use axum::extract::State;
use axum::Json;
use chrono::Utc;
use loomem_core::feedback::{ApplyRatingArgs, FeedbackService, RatingOutcome};
use loomem_core::manifest::{classify_stream, StreamKind};
use loomem_core::source_tag::SourceTag;
use serde::Deserialize;
use serde_json::Value;
use std::sync::Arc;

use super::types::*;
use crate::auth::{AuthContext, KeyScope, StreamMembership};
use crate::handlers;
use crate::AppState;
use loomem_core::storage::UserRole;

/// Result of resolving which stream a tool call should operate on.
///
/// Built by `resolve_stream_for_call` from (args.stream, default, auth.memberships).
/// Carries the effective per-stream `(role, source)` that drives the RBAC gate —
/// NOT the caller's global `auth.role / auth.scope`, because per /31 memberships
/// a caller can have different effective rights on different streams.
#[derive(Debug)]
struct ResolvedStream {
    stream_id: String,
    role: UserRole,
    source: KeyScope,
}

/// Resolve the effective stream for a tool call.
///
/// * If `args.stream` is a non-empty string → verify caller has a matching
///   membership; return `(explicit, membership.role, membership.source)`.
/// * Otherwise (missing, null, or empty string) → return
///   `(default_stream_id, auth.role, auth.scope)` — backward-compat with
///   pre-/32 behavior.
///
/// Returns `Err(ToolResult::error(...))` when the explicit stream is not in
/// `auth.memberships`. Errors are surfaced as ToolResult, not JsonRpcError —
/// matches the pre-/31 pattern where role/scope denials return ToolResult::error
/// so the LLM sees them in `content` rather than as transport-level faults.
fn resolve_stream_for_call(
    args: &Value,
    default_stream_id: &str,
    auth: &AuthContext,
) -> Result<ResolvedStream, ToolResult> {
    let explicit = args
        .get("stream")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());

    let Some(explicit_id) = explicit else {
        return Ok(ResolvedStream {
            stream_id: default_stream_id.to_string(),
            role: auth.role,
            source: auth.scope,
        });
    };

    let Some(membership) = auth.memberships.iter().find(|m| m.stream_id == explicit_id) else {
        return Err(ToolResult::error(format!(
            "Access denied: no membership on stream '{}'.",
            explicit_id
        )));
    };

    Ok(ResolvedStream {
        stream_id: membership.stream_id.clone(),
        role: membership.role,
        source: membership.source,
    })
}

/// Decide whether a call with effective `(role, source)` may execute `tool`.
///
/// Semantics (inheriting §D5 pre-/32 policy, generalized to per-call):
/// * `source=Private` → owner on own stream, no gate. Includes the case where
///   `membership.role == Admin` — this is the /31 Findings owner-equivalent
///   fallback (`UserRole` lacks an `Owner` variant; Admin stands in as
///   write+delete rights marker for private ownership). Critically, this is
///   NOT treated like a Shared+Admin global admin — a user's Private+Admin
///   gives them full rights on their own stream only, and the
///   early-return-on-Private guarantees it.
/// * `source=Shared` → role-gated per §D5, with /33 §3.4 amendment:
///   - `memory_store` / `memory_ingest` / `memory_associate` — `role.can_write()`
///   - `memory_delete` — `role.can_delete_shared()` (Admin-only)
///   - `memory_dream`  — `role.can_dream_shared()` (Admin-only)
///   - all other tools (search / context / profile / status / reflect / graph
///     / history / namespaces) — no gate.
///
/// `memory_associate` joined the write-gated group in cycle /33 §3.4 —
/// deliberate behavior change vs pre-/33: a Reader on a Shared or project
/// stream can no longer run `memory_associate`. Rationale: associate creates
/// new associative edges between chunks, which is a write-style mutation on
/// the stream's graph; leaving it ungated let Readers mutate project data.
///
/// Returns `Some(error)` to deny, `None` to allow.
fn gate_tool(tool: &str, role: UserRole, source: KeyScope) -> Option<ToolResult> {
    if source == KeyScope::Private {
        return None;
    }
    match tool {
        // Cycle /33 §3.4: memory_associate joined write-gated group.
        "memory_store" | "memory_ingest" | "memory_associate" => {
            if role.can_write() {
                None
            } else {
                Some(ToolResult::error(
                    "Read-only access: write operations not permitted for Reader role on shared scope."
                        .to_string(),
                ))
            }
        }
        "memory_delete" => {
            if role.can_delete_shared() {
                None
            } else {
                Some(ToolResult::error(
                    "Admin-only on shared scope: memory_delete requires Admin.".to_string(),
                ))
            }
        }
        "memory_dream" => {
            if role.can_dream_shared() {
                None
            } else {
                Some(ToolResult::error(
                    "Admin-only on shared scope: memory_dream requires Admin.".to_string(),
                ))
            }
        }
        _ => None,
    }
}

pub async fn dispatch_tool(
    state: &Arc<AppState>,
    name: &str,
    args: Value,
    default_stream_id: &str,
    auth: &AuthContext,
) -> Result<ToolResult, JsonRpcError> {
    // Resolve effective stream for this call (explicit `stream` arg or default).
    let resolved = match resolve_stream_for_call(&args, default_stream_id, auth) {
        Ok(r) => r,
        Err(deny) => return Ok(deny),
    };

    // Gate on effective (role, source) — per-membership, not global auth.
    if let Some(err) = gate_tool(name, resolved.role, resolved.source) {
        return Ok(err);
    }

    let stream = resolved.stream_id.as_str();
    match name {
        "memory_store" => tool_store(state, args, stream, auth.user_id.clone()).await,
        "memory_search" => tool_search(state, args, stream).await,
        "memory_context" => tool_context(state, args, stream).await,
        "memory_profile" => tool_profile(state, args, stream).await,
        "memory_status" => tool_status(state, args, stream).await,
        "memory_reflect" => tool_reflect(state, args, stream).await,
        "memory_graph" => tool_graph(state, args, stream).await,
        "memory_namespaces" => tool_namespaces(state, stream, &auth.memberships).await,
        "memory_ingest" => tool_ingest(state, args, stream, auth.user_id.clone()).await,
        "memory_dream" => tool_dream(state, args, stream).await,
        "memory_history" => tool_history(state, args, stream).await,
        "memory_delete" => tool_delete(state, args, stream).await,
        "memory_associate" => tool_associate(state, args, stream).await,
        // Cycle/113: feedback tool.
        "memory_feedback" => tool_feedback(state, args, stream, auth).await,
        _ => Err(JsonRpcError::invalid_params(&format!(
            "Unknown tool: {}",
            name
        ))),
    }
}

// ── memory_store ──────────────────────────────────────────────────

#[derive(Deserialize)]
struct StoreArgs {
    content: String,
    source: Option<String>,
    subject: Option<String>,
    metadata: Option<Value>,
    // /32: stream is extracted upstream by resolve_stream_for_call before
    // deserialization, so this field is never read via StoreArgs — it exists
    // to document the LLM-facing contract and to stay tolerant of serde's
    // default deny_unknown_fields posture if it is ever enabled. Same pattern
    // on every args struct for the 12 stream-accepting tools.
    #[allow(dead_code)]
    #[serde(default)]
    stream: Option<String>,
}

/// Lightweight fact_type classification via keyword matching.
/// Note: event_date extraction is handled by the dream/consolidation worker (requires LLM).
fn classify_fact_type(content: &str) -> loomem_core::storage::FactType {
    let lower = content.to_lowercase();

    // Preference/decision keywords
    let pref_keywords = [
        "prefer",
        "chose",
        "decided",
        "switch",
        "zamiast",
        "woli",
        "ulubion",
        "zdecydow",
        "wybrał",
        "likes",
        "dislikes",
        "favorite",
        "favourite",
    ];
    if pref_keywords.iter().any(|k| lower.contains(k)) {
        return loomem_core::storage::FactType::PreferenceOrDecision;
    }

    // Experience keywords (checked before project_state)
    let exp_keywords = [
        "lesson",
        "learned",
        "always ",
        "never ",
        "best practice",
        "anti-pattern",
        "antipattern",
        "tip:",
        "when you ",
        "lekcja",
        "nauczył",
        "sprawdzon",
        "unikaj",
        "strategia",
    ];
    if exp_keywords.iter().any(|k| lower.contains(k)) {
        return loomem_core::storage::FactType::Experience;
    }

    // Project state keywords
    let proj_keywords = [
        "working on",
        "sprint",
        "deadline",
        "blocked",
        "projekt",
        "pracuje nad",
        "kończy się",
        "release",
        "deploy",
        "migration",
        "bug",
        "task",
    ];
    if proj_keywords.iter().any(|k| lower.contains(k)) {
        return loomem_core::storage::FactType::ProjectState;
    }

    loomem_core::storage::FactType::Fact
}

/// /151 (port of /114b2): map an extracted event date onto the
/// `(extraction_meta.event_date, valid_from)` pair used by `tool_store`.
/// `None` (timeless fact, LLM failure, gate off) falls back to the ingest
/// timestamp; pre-1970 dates cannot be represented in the `u64` field and
/// fall back too.
fn event_date_routing(
    event_date: Option<chrono::NaiveDate>,
    timestamp: u64,
) -> (Option<String>, u64) {
    let iso = event_date.map(|d| d.format("%Y-%m-%d").to_string());
    let unix = event_date
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .and_then(|dt| u64::try_from(dt.and_utc().timestamp()).ok());
    (iso, unix.unwrap_or(timestamp))
}

/// /151 (port of /114b2): build the `ExtractionMeta` + `valid_from` pair for
/// a direct `memory_store` write. Probes the LLM for an absolute event date
/// when both gates are on: `knowledge_extraction.enabled` (config) AND
/// `LOOMEM_EVENT_DATE_EXTRACTION` env (default OFF — /151 scope extension
/// per CLAUDE.md §9.5). Probe failure → `None` → `valid_from` falls back to
/// the ingest timestamp, byte-identical to pre-/151 behavior.
async fn store_extraction_meta(
    state: &Arc<AppState>,
    content: &str,
    subject: String,
    fact_type: loomem_core::storage::FactType,
    timestamp: u64,
) -> (loomem_core::storage::ExtractionMeta, u64) {
    let probe_enabled =
        state.config.knowledge_extraction.enabled && loomem_core::event_date::extraction_enabled();
    let event_date = if probe_enabled {
        let anchor_date = Utc::now().format("%Y-%m-%d").to_string();
        loomem_core::event_date::extract_event_date(
            &state.http_client,
            &state.config.llm,
            &state.config.knowledge_extraction.model,
            content,
            &anchor_date,
        )
        .await
    } else {
        None
    };
    let (event_date_iso, valid_from_ts) = event_date_routing(event_date, timestamp);
    let meta = loomem_core::storage::ExtractionMeta {
        fact_type,
        subject: Some(subject),
        event_date: event_date_iso,
        event_date_context: None,
        supersedes: None,
        superseded_by: None,
        confidence: 0.7,
        extracted_from: None,
        // Model recorded only when the probe actually ran — gate off keeps
        // the pre-/151 None (no LLM touched this chunk's metadata).
        extraction_model: probe_enabled.then(|| state.config.knowledge_extraction.model.clone()),
    };
    (meta, valid_from_ts)
}

async fn tool_store(
    state: &Arc<AppState>,
    args: Value,
    stream_id: &str,
    user_id: Option<String>,
) -> Result<ToolResult, JsonRpcError> {
    let args: StoreArgs =
        serde_json::from_value(args).map_err(|e| JsonRpcError::invalid_params(&e.to_string()))?;

    // Content size limit
    if args.content.len() > 102_400 {
        return Ok(ToolResult::error(format!(
            "Content too large: {} bytes (max 102400)",
            args.content.len()
        )));
    }

    let id = uuid::Uuid::new_v4().to_string();
    let timestamp = Utc::now().timestamp() as u64;
    let stream = stream_id.to_string();
    let subject = args.subject.unwrap_or_else(|| "user".into());
    let source = args.source.unwrap_or_else(|| "mcp".into());
    let fact_type = classify_fact_type(&args.content);
    let is_preference = matches!(
        fact_type,
        loomem_core::storage::FactType::PreferenceOrDecision
    );

    // /151 (port of /114b2) — gated LLM event_date probe + valid_from
    // routing; see store_extraction_meta.
    let (extraction_meta, valid_from_ts) =
        store_extraction_meta(state, &args.content, subject, fact_type, timestamp).await;

    let chunk = loomem_core::storage::Chunk {
        id: id.clone(),
        content: args.content.clone(),
        stream: stream.clone(),
        level: 0,
        score: 1.0,
        timestamp,
        consolidated: false,
        dormant: false,
        in_progress: false,
        prompt_version: None,
        source_ids: None,
        last_decay: None,
        metadata: args.metadata.clone(),
        importance: Some(if is_preference { 2.0 } else { 1.0 }),
        persistent: true,
        last_implicit_boost: None,
        access_count: 0,
        source: Some(SourceTag::from_agent(source)),
        created_by: Some("mcp".into()),
        updated_at: Some(timestamp),
        valid_from: Some(valid_from_ts),
        valid_until: None,
        is_latest: true,
        superseded_by: None,
        supersedes_id: None,
        root_memory_id: None,
        version: 1,
        memory_type: if is_preference {
            Some("static".to_string())
        } else {
            None
        },
        extraction_meta: Some(extraction_meta),
        deleted_at: None,
        trust_level: Some("a2".to_string()), // MCP = assistant-generated
        ingester_user_id: user_id,

        alpha: 1.0,
        beta: 1.0,
        harmful_count: 0,
        n_ratings: 0,
        last_rated_at: None,
    };

    let content = args.content.clone();
    match handlers::ingest::persist_chunk(
        state,
        chunk,
        &content,
        "default",
        "default",
        &stream,
        0,
        timestamp as i64,
        None,
        args.metadata.as_ref(),
    )
    .await
    {
        Ok(stored_id) => {
            let preview: String = args.content.chars().take(80).collect();
            Ok(ToolResult::text(format!(
                "Stored: \"{}...\" (id: {})",
                preview, stored_id
            )))
        }
        Err(e) => Ok(ToolResult::error(format!("Failed to store: {:?}", e))),
    }
}

// ── memory_search ─────────────────────────────────────────────────

#[derive(Deserialize)]
struct SearchArgs {
    query: String,
    top_k: Option<usize>,
    time_filter: Option<String>,
    /// Bitemporal time-travel: return chunks whose `[valid_from, valid_until]`
    /// interval covers this unix timestamp (seconds). Cycle /42 D3.
    #[serde(default)]
    valid_at: Option<u64>,
    /// Include superseded chunks in results (default false). Cycle /42 D3.
    #[serde(default)]
    include_superseded: Option<bool>,
    #[allow(dead_code)]
    #[serde(default)]
    stream: Option<String>,
}

/// /142 + /143 (ADR-017): format the content-type tag ` [content_type]` for an
/// MCP result line, e.g. ` [case_study]`. Empty when the result carries no
/// classification (no sidecar hit). Since /143 the band/source are dropped from
/// the tag — `other` already signals uncertainty and source is always `llm`.
fn content_type_tag(content_type: Option<&str>) -> String {
    match content_type {
        Some(ct) => format!(" [{ct}]"),
        None => String::new(),
    }
}

async fn tool_search(
    state: &Arc<AppState>,
    args: Value,
    stream_id: &str,
) -> Result<ToolResult, JsonRpcError> {
    let args: SearchArgs =
        serde_json::from_value(args).map_err(|e| JsonRpcError::invalid_params(&e.to_string()))?;

    // Detect aggregation queries for enumeration hint + top_k boost
    let query_lower = args.query.to_lowercase();
    let is_aggregation = [
        "ile ",
        "how many",
        "ile razy",
        "ilu ",
        "lista ",
        "list all",
        "wymień",
        "wypisz",
        "wszystkie ",
        "wszystkich ",
        "ile różnych",
        "ile unikalnych",
    ]
    .iter()
    .any(|k| query_lower.contains(k));

    let top_k = if is_aggregation {
        args.top_k.unwrap_or(30).min(30)
    } else {
        args.top_k.unwrap_or(5).min(20)
    };

    let req = handlers::types::SearchRequest {
        query: args.query,
        user_id: None,
        top_k: Some(top_k),
        stream: Some(stream_id.to_string()),
        streams: None,
        entity: None,
        date_from: args.time_filter,
        date_to: None,
        valid_at: args.valid_at,
        dry_run: true,
        filters: None,
        include_superseded: args.include_superseded.unwrap_or(false),
        trace: false,
        fact_type: None,
        subject: None,
        min_confidence: None,
        include_associations: false,
        source_agent: None,
        exclude_source_agents: None,
        scope: None,
        debug_query_classification: false,
        debug_signal_breakdown: false,
    };

    // Construct auth context from MCP-authenticated stream_id
    // Internal synthetic auth for a nested handler call. The caller's real
    // auth was already enforced by the outer MCP request handler; this stub
    // is server-originated (user_id = None) so it carries Admin+Shared per
    // brief §G3 to avoid spurious role gate failures. stream_id is whatever
    // the outer caller's derived stream was (DEFAULT_STREAM_ID for Shared,
    // user.stream_id for Private — handed through verbatim).
    let auth = crate::auth::AuthContext::single_stream(
        stream_id.to_string(),
        loomem_core::storage::UserRole::Admin,
        crate::auth::KeyScope::Shared,
        None,
        true,
    );
    match handlers::search::search_handler(State(state.clone()), axum::Extension(auth), Json(req))
        .await
    {
        Ok(Json(resp)) => {
            if resp.results.is_empty() {
                return Ok(ToolResult::text("No relevant memories found."));
            }
            let mut text = if is_aggregation {
                format!("ENUMERATION QUERY: Carefully count ALL unique items below. Do not estimate — enumerate each one.\n\nFound {} relevant memories:\n\n", resp.results.len())
            } else {
                format!("Found {} relevant memories:\n\n", resp.results.len())
            };
            for (i, r) in resp.results.iter().enumerate() {
                // Prefer event_date from extraction_meta, fall back to ingestion timestamp
                let chunk_opt = state.store.get_chunk(&r.id).ok().flatten();
                let event_date = chunk_opt
                    .as_ref()
                    .and_then(|c| c.extraction_meta.as_ref())
                    .and_then(|m| m.event_date.as_ref())
                    .cloned();
                let ts = event_date.unwrap_or_else(|| {
                    r.metadata
                        .as_ref()
                        .and_then(|m| m.get("timestamp"))
                        .and_then(|v| v.as_i64())
                        .map(|t| {
                            chrono::DateTime::from_timestamp(t, 0)
                                .map(|d| d.format("%Y-%m-%d").to_string())
                                .unwrap_or_default()
                        })
                        .unwrap_or_default()
                });

                let supersedes_note = chunk_opt
                    .as_ref()
                    .and_then(|c| c.supersedes_id.as_ref())
                    .map(|_| " [UPDATED — supersedes older version]")
                    .unwrap_or("");

                // Cycle/142 + /143: content-type tag `[content_type]`, e.g.
                // `[case_study]`. Present only when the result was classified
                // (sidecar hit). Band/source dropped in /143 (ADR-017 Amd v2).
                let content_type_tag = content_type_tag(r.content_type.as_deref());

                text.push_str(&format!(
                    "{}. [{}]{} {}{} (score: {:.2})\n",
                    i + 1,
                    ts,
                    content_type_tag,
                    r.content,
                    supersedes_note,
                    r.score
                ));
            }
            text.push_str("\nNote: When facts have dates, ALWAYS prefer the most recent version. Items marked [UPDATED] supersede older versions.");
            Ok(ToolResult::text(text))
        }
        Err(e) => Ok(ToolResult::error(format!("Search failed: {:?}", e))),
    }
}

// ── memory_context ────────────────────────────────────────────────

#[derive(Deserialize)]
struct ContextArgs {
    query: Option<String>,
    budget_tokens: Option<usize>,
    sections: Option<Vec<String>>,
    #[allow(dead_code)]
    #[serde(default)]
    stream: Option<String>,
}

async fn tool_context(
    state: &Arc<AppState>,
    args: Value,
    stream_id: &str,
) -> Result<ToolResult, JsonRpcError> {
    let args: ContextArgs =
        serde_json::from_value(args).map_err(|e| JsonRpcError::invalid_params(&e.to_string()))?;

    let req = handlers::types::ContextPackRequest {
        query: args.query,
        stream: Some(stream_id.to_string()),
        budget_tokens: Some(args.budget_tokens.unwrap_or(2000).min(8000)),
        sections: args.sections,
        format: Some("markdown".into()),
    };

    // Synthetic auth scoped to the MCP caller's stream — matches the
    // stream we're requesting so validate_stream passes.
    // Internal synthetic auth for a nested handler call. The caller's real
    // auth was already enforced by the outer MCP request handler; this stub
    // is server-originated (user_id = None) so it carries Admin+Shared per
    // brief §G3 to avoid spurious role gate failures. stream_id is whatever
    // the outer caller's derived stream was (DEFAULT_STREAM_ID for Shared,
    // user.stream_id for Private — handed through verbatim).
    let auth = crate::auth::AuthContext::single_stream(
        stream_id.to_string(),
        loomem_core::storage::UserRole::Admin,
        crate::auth::KeyScope::Shared,
        None,
        true,
    );
    match handlers::context::context_pack_handler(
        State(state.clone()),
        axum::Extension(auth),
        Json(req),
    )
    .await
    {
        Ok(Json(resp)) => Ok(ToolResult::text(resp.context)),
        Err(e) => Ok(ToolResult::error(format!("Context pack failed: {:?}", e))),
    }
}

// ── memory_profile ────────────────────────────────────────────────

#[derive(Deserialize)]
struct ProfileArgs {
    format: Option<String>,
    refresh: Option<bool>,
    #[allow(dead_code)]
    #[serde(default)]
    stream: Option<String>,
}

async fn tool_profile(
    state: &Arc<AppState>,
    args: Value,
    stream_id: &str,
) -> Result<ToolResult, JsonRpcError> {
    let stream = stream_id.to_string();
    let parsed: ProfileArgs = serde_json::from_value(args).unwrap_or(ProfileArgs {
        format: None,
        refresh: None,
        stream: None,
    });
    let fmt = parsed.format.as_deref().unwrap_or("markdown");
    let force_refresh = parsed.refresh.unwrap_or(false);

    // ADR-014 / cycle/139: stream-kind-aware. Private streams keep the
    // untouched UserProfile path; shared/project streams get a StreamManifest
    // (knowledge-base dossier, not a person). Routing lives in the server-layer
    // helper so this orchestrator stays thin.
    match crate::manifest::build_profile_or_manifest(state, &stream, force_refresh).await {
        Ok(result) => {
            let text = match fmt {
                "json" => serde_json::to_string_pretty(&result).unwrap_or_default(),
                _ => result.to_markdown(),
            };
            Ok(ToolResult::text(text))
        }
        Err(e) => Ok(ToolResult::error(format!(
            "Profile generation failed: {}",
            e
        ))),
    }
}

// ── memory_status ─────────────────────────────────────────────────

#[derive(Deserialize)]
struct StatusArgs {
    // /32: stream extracted upstream by resolve_stream_for_call; present to
    // mirror the shape of the other 11 stream-accepting args structs.
    #[allow(dead_code)]
    #[serde(default)]
    stream: Option<String>,
}

async fn tool_status(
    state: &Arc<AppState>,
    args: Value,
    stream_id: &str,
) -> Result<ToolResult, JsonRpcError> {
    let _args: StatusArgs = serde_json::from_value(args).unwrap_or(StatusArgs { stream: None });
    // Count chunks across all levels for this stream
    let mut chunk_count = 0usize;
    for level in 0..=2 {
        let prefix = format!("chunk:L{}:", level);
        for (_key, value) in state.store.prefix_scan(prefix.as_bytes()) {
            if let Ok(chunk) = state.store.decode_chunk(&value) {
                if chunk.stream == stream_id && chunk.is_latest && chunk.deleted_at.is_none() {
                    chunk_count += 1;
                }
            }
        }
    }

    // Count embeddings
    let embedding_count = state.store.count_embeddings().unwrap_or(0);

    // Count clusters
    let cluster_count = state.store.prefix_scan(b"assoc:centroid:").count();

    // Associator status
    let assoc_status = if state.config.associator.enabled {
        if cluster_count > 0 {
            format!("active ({} clusters)", cluster_count)
        } else {
            "enabled, awaiting clustering".to_string()
        }
    } else {
        "disabled".to_string()
    };

    // /150b Gap 6: surface process-wide log-loss counters (0 in healthy
    // operation; non-zero signals the event log / audit trail is incomplete).
    let event_log_drops = loomem_core::event_log::emit_drop_count();
    let audit_write_failures = loomem_core::audit::append_failure_count();

    // /157 S3: backlog + LLM failure visibility (incidents A/B, 2026-06-11).
    let undecodable = state.store.last_scan_decode_summary().map_or_else(
        || "n/a (no full scan yet)".to_string(),
        |s| s.undecodable.to_string(),
    );
    let llm_fail = loomem_core::llm_failures::global().recent();

    let text = format!(
        "Loomem Status: ok\nYour stream: {stream_id}\nYour memories: {chunk_count}\nEmbeddings (global): {embedding_count}\nAssociator: {assoc_status}\nEvent log drops: {event_log_drops}\nAudit write failures: {audit_write_failures}\nUndecodable chunks (last full scan): {undecodable}\nLLM failures (last {}m): extraction={}, ner={}, embedding={}, consolidation={}",
        llm_fail.window_secs / 60,
        llm_fail.extraction,
        llm_fail.ner,
        llm_fail.embedding,
        llm_fail.consolidation,
    );
    Ok(ToolResult::text(text))
}

// ── memory_reflect ───────────────────────────────────────────────

#[derive(Deserialize)]
struct ReflectArgs {
    max_chunks: Option<usize>,
    #[allow(dead_code)]
    #[serde(default)]
    stream: Option<String>,
}

async fn tool_reflect(
    state: &Arc<AppState>,
    args: Value,
    stream_id: &str,
) -> Result<ToolResult, JsonRpcError> {
    let args: ReflectArgs = serde_json::from_value(args).unwrap_or(ReflectArgs {
        max_chunks: None,
        stream: None,
    });
    let max_chunks = args.max_chunks.unwrap_or(200);

    // Collect chunks from this stream
    let mut chunks: Vec<loomem_core::storage::Chunk> = Vec::new();
    for level in 0..=2 {
        let prefix = format!("chunk:L{}:", level);
        for (_key, value) in state.store.prefix_scan(prefix.as_bytes()) {
            if let Ok(chunk) = state.store.decode_chunk(&value) {
                if chunk.stream == stream_id && chunk.is_latest {
                    chunks.push(chunk);
                }
            }
            if chunks.len() >= max_chunks {
                break;
            }
        }
    }

    if chunks.is_empty() {
        return Ok(ToolResult::text("No memories found in this stream."));
    }

    // Analyze quality
    let total = chunks.len();
    let mut no_subject = 0;
    let mut no_extraction_meta = 0;
    let mut low_confidence = 0;
    let mut _superseded = 0;
    let mut raw_transcripts = 0;
    let mut short_chunks = 0;
    let mut by_source: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut by_type: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut by_level: std::collections::HashMap<i32, usize> = std::collections::HashMap::new();

    for chunk in &chunks {
        *by_level.entry(chunk.level).or_default() += 1;
        let src = chunk
            .source
            .as_ref()
            .map(|s| s.agent.as_str())
            .unwrap_or("unknown");
        *by_source.entry(src.to_string()).or_default() += 1;

        if let Some(ref meta) = chunk.extraction_meta {
            if meta.subject.is_none() {
                no_subject += 1;
            }
            if meta.confidence < 0.5 {
                low_confidence += 1;
            }
            let ft = match meta.fact_type {
                loomem_core::storage::FactType::PreferenceOrDecision => "preference_or_decision",
                loomem_core::storage::FactType::ProjectState => "project_state",
                loomem_core::storage::FactType::Fact => "fact",
                loomem_core::storage::FactType::Event => "event",
            };
                loomem_core::storage::FactType::Experience => "experience",
            *by_type.entry(ft.to_string()).or_default() += 1;
        } else {
            no_extraction_meta += 1;
        }

        if !chunk.is_latest {
            _superseded += 1;
        }
        if src == "raw-transcript" || src == "openclaw-memory" {
            raw_transcripts += 1;
        }
        if chunk.content.split_whitespace().count() < 5 {
            short_chunks += 1;
        }
    }

    let quality_score = {
        let has_meta_pct = (total - no_extraction_meta) as f64 / total as f64;
        let has_subject_pct = (total - no_subject - no_extraction_meta) as f64 / total as f64;
        let structured_pct = 1.0 - (raw_transcripts as f64 / total as f64);
        ((has_meta_pct * 0.4 + has_subject_pct * 0.3 + structured_pct * 0.3) * 100.0) as u32
    };

    let mut report = format!(
        "## Memory Quality Report\n\n**Score: {}%** (analyzed {} chunks)\n\n",
        quality_score, total
    );

    report.push_str("### Breakdown\n");
    report.push_str(&format!(
        "- Structured facts (with extraction_meta): {}/{}\n",
        total - no_extraction_meta,
        total
    ));
    report.push_str(&format!(
        "- With subject tag: {}/{}\n",
        total - no_subject - no_extraction_meta,
        total
    ));
    report.push_str(&format!("- Low confidence (<0.5): {}\n", low_confidence));
    report.push_str(&format!(
        "- Raw transcripts (unprocessed): {}\n",
        raw_transcripts
    ));
    report.push_str(&format!("- Very short (<5 words): {}\n", short_chunks));

    report.push_str("\n### By level\n");
    for (level, count) in by_level.iter() {
        report.push_str(&format!("- L{}: {}\n", level, count));
    }

    report.push_str("\n### By source\n");
    let mut sources: Vec<_> = by_source.iter().collect();
    sources.sort_by(|a, b| b.1.cmp(a.1));
    for (src, count) in sources {
        report.push_str(&format!("- {}: {}\n", src, count));
    }

    if !by_type.is_empty() {
        report.push_str("\n### By fact type\n");
        for (ft, count) in &by_type {
            report.push_str(&format!("- {}: {}\n", ft, count));
        }
    }

    // Suggestions
    report.push_str("\n### Suggestions\n");
    if no_extraction_meta > total / 3 {
        report.push_str(&format!(
            "- **Run reprocess-legacy**: {} chunks lack extraction metadata. POST /v1/reprocess-legacy\n",
            no_extraction_meta
        ));
    }
    if raw_transcripts > total / 4 {
        report.push_str(&format!(
            "- **Too many raw transcripts**: {} unprocessed. Use memory_ingest instead of memory_store for conversations.\n",
            raw_transcripts
        ));
    }
    if low_confidence > 10 {
        report.push_str(&format!(
            "- **Low confidence facts**: {} chunks below 0.5 confidence. Consider dream consolidation.\n",
            low_confidence
        ));
    }
    if short_chunks > 10 {
        report.push_str(&format!(
            "- **Noise**: {} very short chunks (<5 words). May be fragments or errors.\n",
            short_chunks
        ));
    }
    if quality_score >= 80 {
        report.push_str(
            "- Memory quality is good. Keep using memory_ingest for new conversations.\n",
        );
    }

    Ok(ToolResult::text(report))
}

// ── memory_graph ─────────────────────────────────────────────────

#[derive(Deserialize)]
struct GraphArgs {
    entity: String,
    #[allow(dead_code)]
    #[serde(default)]
    stream: Option<String>,
}

async fn tool_graph(
    state: &Arc<AppState>,
    args: Value,
    stream_id: &str,
) -> Result<ToolResult, JsonRpcError> {
    let args: GraphArgs =
        serde_json::from_value(args).map_err(|e| JsonRpcError::invalid_params(&e.to_string()))?;

    match state.graph.get_entity_by_name(&args.entity, stream_id) {
        Ok(Some(entity)) => {
            let mut text = format!(
                "Entity: {} (type: {})\n",
                entity.canonical_name, entity.entity_type
            );

            if !entity.aliases.is_empty() {
                text.push_str(&format!("Aliases: {}\n", entity.aliases.join(", ")));
            }
            text.push_str(&format!("Linked chunks: {}\n\n", entity.chunk_ids.len()));

            // Neighbors are already stream-scoped via entity lookup
            match state.graph.get_neighbors(&entity.id) {
                Ok(neighbors) => {
                    if neighbors.is_empty() {
                        text.push_str("No connections found.\n");
                    } else {
                        text.push_str("Connections:\n");
                        for (edge, target) in &neighbors {
                            text.push_str(&format!(
                                "  → {} ({}) [{}]\n",
                                target.canonical_name, edge.relation_type, target.entity_type
                            ));
                        }
                    }
                }
                Err(_) => text.push_str("Could not load connections.\n"),
            }

            Ok(ToolResult::text(text))
        }
        Ok(None) => Ok(ToolResult::text(format!(
            "Entity '{}' not found in knowledge graph.",
            args.entity
        ))),
        Err(e) => Ok(ToolResult::error(format!("Graph query failed: {}", e))),
    }
}

// ── memory_namespaces ────────────────────────────────────────────

/// Map a `(source, role)` membership pair to an access label for the
/// namespace listing. Private memberships always report `owner`
/// (per /31 Findings: UserRole::Admin is the owner-equivalent for private
/// streams, not global admin). Shared memberships report the role-derived
/// capability level.
fn access_label(source: KeyScope, role: UserRole) -> &'static str {
    if source == KeyScope::Private {
        return "owner";
    }
    if role.can_delete_shared() {
        "admin"
    } else if role.can_write() {
        "write"
    } else {
        "read"
    }
}

/// Resolve `(display_alias, type_label)` for a stream in the namespace listing
/// (ADR-016). The type label is derived deterministically from `classify_stream`.
///
/// PRIVACY GUARD (ADR-016 / ADR-009): the `Private` branch performs **no**
/// per-user name lookup — `__user_<uuid>` streams resolve only to a configured
/// alias or a generic label, never to a person or email. Aliases come from
/// `config.namespaces`.
fn alias_and_type(
    kind: StreamKind,
    stream_id: &str,
    alias_map: &std::collections::HashMap<String, String>,
) -> (String, &'static str) {
    // `config.namespaces` is keyed name -> stream_id, so match on the value.
    let config_alias = || {
        alias_map
            .iter()
            .find(|(_, sid)| sid.as_str() == stream_id)
            .map(|(name, _)| name.clone())
    };
    match kind {
        StreamKind::Project => {
            let alias = config_alias().unwrap_or_else(|| stream_id.to_string());
            (alias, "project")
        }
        StreamKind::Shared => (
            config_alias().unwrap_or_else(|| "Shared memory".to_string()),
            "organization_shared",
        ),
        // Privacy guard: NEVER map __user_<uuid> to a person — generic alias only.
        StreamKind::Private => (
            config_alias().unwrap_or_else(|| "Your private memory".to_string()),
            "user_private",
        ),
    }
}

async fn tool_namespaces(
    state: &Arc<AppState>,
    default_stream_id: &str,
    memberships: &[StreamMembership],
) -> Result<ToolResult, JsonRpcError> {
    let alias_map = &state.config.namespaces;
    let mut text = "Your namespaces:\n\n".to_string();

    if memberships.is_empty() {
        // /32 invariant (from /31 AC-2) guarantees at least one membership, but
        // guard defensively so the tool never panics — an empty list just
        // reports no accessible streams.
        text.push_str("(no accessible streams)\n");
        return Ok(ToolResult::text(text));
    }

    for m in memberships {
        let is_default = m.stream_id == default_stream_id;
        let access = access_label(m.source, m.role);

        // Alias + type (ADR-016): project name / shared alias / generic private.
        let kind = classify_stream(&m.stream_id);
        let (display_name, type_label) = alias_and_type(kind, &m.stream_id, alias_map);

        text.push_str(&format!(
            "- {} (stream_id: {}, type: {}, access: {}{})\n",
            display_name,
            m.stream_id,
            type_label,
            access,
            if is_default { ", default" } else { "" }
        ));
    }

    Ok(ToolResult::text(text))
}

// ── memory_ingest ────────────────────────────────────────────────

#[derive(Deserialize)]
struct IngestArgs {
    content: String,
    conversation_date: Option<String>,
    #[allow(dead_code)]
    #[serde(default)]
    stream: Option<String>,
}

/// /151 (port of /114b1): per-fact `ExtractionMeta` + `valid_from` routing
/// for the extract_knowledge branch of `tool_ingest` (parity with
/// workspace.rs `ingest_conversation`). Meta is built once and reused for
/// both `valid_from` and the chunk's `extraction_meta` field; facts without
/// an extracted event_date fall back to the ingest timestamp.
fn ingest_fact_meta(
    fact: &loomem_core::memory_extractor::ExtractedFact,
    model: &str,
    timestamp: u64,
) -> (loomem_core::storage::ExtractionMeta, u64) {
    let meta = fact.to_extraction_meta(None, model);
    let valid_from = meta.event_date_unix().unwrap_or(timestamp);
    (meta, valid_from)
}

/// /157 S1 (AC-1/AC-2): user-facing summary for an (at least partially)
/// successful extraction. Zero facts with zero failures reads exactly as
/// before; any per-chunk failures append a warning so partial success is
/// visible instead of silently under-reporting.
fn ingest_summary(
    outcome: &loomem_core::memory_extractor::ExtractionOutcome,
    stored: usize,
    skipped: usize,
) -> String {
    let mut msg = format!(
        "Extracted {} facts from conversation, stored {}, skipped {}.",
        outcome.facts.len(),
        stored,
        skipped
    );
    if let Some(first) = outcome.failures.first() {
        msg.push_str(&format!(
            " Warning: {} extraction chunk(s) failed; facts may be incomplete. First error ({}): {}",
            outcome.failures.len(),
            first.status_label(),
            first.reason
        ));
    }
    msg
}

/// /157 S1 (AC-1): extraction failed outright — surface status + reason,
/// never the "Extracted 0 facts" success shape.
fn extraction_failed_message(err: &loomem_core::memory_extractor::ExtractionError) -> String {
    let (status, reason) = err.status_and_reason();
    format!("Extraction failed ({status}): {reason}. No facts stored.")
}

async fn tool_ingest(
    state: &Arc<AppState>,
    args: Value,
    stream_id: &str,
    user_id: Option<String>,
) -> Result<ToolResult, JsonRpcError> {
    let args: IngestArgs =
        serde_json::from_value(args).map_err(|e| JsonRpcError::invalid_params(&e.to_string()))?;

    let today = args
        .conversation_date
        .unwrap_or_else(|| Utc::now().format("%Y-%m-%d").to_string());

    // Use knowledge extraction if enabled
    if state.config.knowledge_extraction.enabled {
        // /157 S1: missing key is a loud config error, not "Extracted 0".
        let Some(api_key) = state.config.llm.get_api_key() else {
            return Ok(ToolResult::error(extraction_failed_message(
                &loomem_core::memory_extractor::ExtractionError::NoApiKey,
            )));
        };
        let chat =
            loomem_core::memory_extractor::HttpExtractionChat::new(&state.http_client, api_key);
        tool_ingest_extract(&chat, state, &args.content, &today, stream_id, user_id).await
    } else {
        tool_ingest_raw(state, &args.content, stream_id, user_id).await
    }
}

/// Extraction branch of `tool_ingest` with an injected chat transport
/// (/157 S1). The seam lets the integration tests below simulate API
/// failures (AC-1) without real HTTP.
async fn tool_ingest_extract(
    chat: &impl loomem_core::memory_extractor::ExtractionChat,
    state: &Arc<AppState>,
    content: &str,
    today: &str,
    stream_id: &str,
    user_id: Option<String>,
) -> Result<ToolResult, JsonRpcError> {
    match loomem_core::memory_extractor::extract_knowledge_with(
        chat,
        &state.config.knowledge_extraction,
        content,
        today,
    )
    .await
    {
        Ok(outcome) => {
            let mut stored = 0;
            let mut skipped = 0;
            for fact in &outcome.facts {
                let id = uuid::Uuid::new_v4().to_string();
                let timestamp = Utc::now().timestamp() as u64;
                // /151 (port of /114b1) — event_date → valid_from
                // routing; see ingest_fact_meta.
                let (extraction_meta_for_chunk, valid_from_ts) =
                    ingest_fact_meta(fact, &state.config.knowledge_extraction.model, timestamp);

                let chunk = loomem_core::storage::Chunk {
                    id: id.clone(),
                    content: fact.content.clone(),
                    stream: stream_id.to_string(),
                    level: 1,
                    score: 1.0,
                    timestamp,
                    consolidated: false,
                    dormant: false,
                    in_progress: false,
                    prompt_version: None,
                    source_ids: None,
                    last_decay: None,
                    metadata: None,
                    importance: Some(if fact.fact_type == "preference_or_decision" {
                        2.0_f64.max(fact.confidence)
                    } else {
                        fact.confidence
                    }),
                    persistent: true,
                    last_implicit_boost: None,
                    access_count: 0,
                    source: Some(SourceTag::from_agent("mcp-ingest")),
                    created_by: Some("mcp".to_string()),
                    updated_at: Some(timestamp),
                    valid_from: Some(valid_from_ts),
                    valid_until: None,
                    is_latest: true,
                    superseded_by: None,
                    supersedes_id: None,
                    root_memory_id: None,
                    version: 1,
                    memory_type: Some(
                        match fact.fact_type.as_str() {
                            "fact" | "preference_or_decision" | "experience" => "static",
                            _ => "dynamic",
                        }
                        .to_string(),
                    ),
                    extraction_meta: Some(extraction_meta_for_chunk),
                    deleted_at: None,
                    trust_level: Some("a2".to_string()),
                    ingester_user_id: user_id.clone(),

                    alpha: 1.0,
                    beta: 1.0,
                    harmful_count: 0,
                    n_ratings: 0,
                    last_rated_at: None,
                };

                match handlers::ingest::persist_chunk(
                    state,
                    chunk,
                    &fact.content,
                    "default",
                    "default",
                    stream_id,
                    1,
                    timestamp as i64,
                    None,
                    None,
                )
                .await
                {
                    Ok(_) => stored += 1,
                    Err(_) => skipped += 1,
                }
            }
            Ok(ToolResult::text(ingest_summary(&outcome, stored, skipped)))
        }
        Err(e) => Ok(ToolResult::error(extraction_failed_message(&e))),
    }
}

/// Raw-storage branch of `tool_ingest` (knowledge extraction disabled).
/// Body unchanged by /157 — split out of the former inline `else`.
async fn tool_ingest_raw(
    state: &Arc<AppState>,
    content: &str,
    stream_id: &str,
    user_id: Option<String>,
) -> Result<ToolResult, JsonRpcError> {
    // Fallback: store raw content
    let id = uuid::Uuid::new_v4().to_string();
    let timestamp = Utc::now().timestamp() as u64;
    let chunk = loomem_core::storage::Chunk {
        id: id.clone(),
        content: content.to_string(),
        stream: stream_id.to_string(),
        level: 0,
        score: 1.0,
        timestamp,
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
        source: Some(SourceTag::from_agent("mcp-ingest-raw")),
        created_by: Some("mcp".to_string()),
        updated_at: Some(timestamp),
        valid_from: Some(timestamp),
        valid_until: None,
        is_latest: true,
        superseded_by: None,
        supersedes_id: None,
        root_memory_id: None,
        version: 1,
        memory_type: None,
        extraction_meta: None,
        deleted_at: None,
        trust_level: Some("a2".to_string()),
        ingester_user_id: user_id,

        alpha: 1.0,
        beta: 1.0,
        harmful_count: 0,
        n_ratings: 0,
        last_rated_at: None,
    };
    match handlers::ingest::persist_chunk(
        state,
        chunk,
        content,
        "default",
        "default",
        stream_id,
        0,
        timestamp as i64,
        None,
        None,
    )
    .await
    {
        Ok(stored_id) => Ok(ToolResult::text(format!(
            "Stored raw transcript (id: {}). Knowledge extraction disabled.",
            stored_id
        ))),
        Err(e) => Ok(ToolResult::error(format!("Failed to store: {:?}", e))),
    }
}

// ── memory_dream ─────────────────────────────────────────────────

#[derive(Deserialize)]
struct DreamArgs {
    // /32: stream extracted upstream by resolve_stream_for_call; present to
    // mirror the shape of the other 11 stream-accepting args structs.
    #[allow(dead_code)]
    #[serde(default)]
    stream: Option<String>,
}

async fn tool_dream(
    state: &Arc<AppState>,
    args: Value,
    stream_id: &str,
) -> Result<ToolResult, JsonRpcError> {
    let _args: DreamArgs = serde_json::from_value(args).unwrap_or(DreamArgs { stream: None });
    if !state.config.dream.enabled {
        return Ok(ToolResult::text("Dream worker is disabled in config."));
    }

    let cost_tracker = loomem_core::CostTracker::new(
        state.store.clone(),
        state.config.cost.clone(),
        state.http_client.clone(),
    );

    match loomem_core::dream::dream_run(
        &state.store,
        &state.tantivy,
        &state.http_client,
        loomem_core::dream::DreamRunContext {
            llm_config: &state.config.llm,
            dream_config: &state.config.dream,
            intent_log: state.intent_log.as_deref(),
        },
        &cost_tracker,
        stream_id,
    )
    .await
    {
        Ok(result) => {
            let text = format!(
                "Dream consolidation complete:\n- Chunks processed: {}\n- Subject groups: {}\n- Facts merged: {}\n- Contradictions resolved: {}\n- Cost: ${:.3}{}",
                result.chunks_processed,
                result.groups_found,
                result.facts_merged,
                result.contradictions_resolved,
                result.cost_usd,
                if result.cost_cap_reached { "\n⚠ Cost cap reached" } else { "" },
            );
            Ok(ToolResult::text(text))
        }
        Err(e) => Ok(ToolResult::error(format!("Dream failed: {}", e))),
    }
}

// ── memory_history ───────────────────────────────────────────────

#[derive(Deserialize)]
struct HistoryArgs {
    chunk_id: String,
    limit: Option<usize>,
    #[allow(dead_code)]
    #[serde(default)]
    stream: Option<String>,
}

async fn tool_history(
    state: &Arc<AppState>,
    args: Value,
    stream_id: &str,
) -> Result<ToolResult, JsonRpcError> {
    let args: HistoryArgs =
        serde_json::from_value(args).map_err(|e| JsonRpcError::invalid_params(&e.to_string()))?;

    // Validate chunk ownership
    if let Ok(Some(chunk)) = state.store.get_chunk(&args.chunk_id) {
        if chunk.stream != stream_id {
            return Ok(ToolResult::error(
                "Access denied: this memory belongs to another stream.".to_string(),
            ));
        }
    }

    let limit = args.limit.unwrap_or(20).min(50);

    match loomem_core::contradiction::get_memory_chain(&state.store, &args.chunk_id, limit) {
        Ok(chain) => {
            if chain.is_empty() {
                return Ok(ToolResult::text(format!(
                    "No version chain found for {}.",
                    args.chunk_id
                )));
            }
            let mut text = format!("Version history ({} versions):\n\n", chain.len());
            for (i, c) in chain.iter().enumerate() {
                let marker = if c.is_latest {
                    " [CURRENT]"
                } else {
                    " [superseded]"
                };
                text.push_str(&format!(
                    "v{}{}: {}\n  id: {}\n",
                    c.version, marker, c.content, c.id
                ));
                if i < chain.len() - 1 {
                    text.push_str("  ↓\n");
                }
            }
            Ok(ToolResult::text(text))
        }
        Err(e) => Ok(ToolResult::error(format!("History failed: {}", e))),
    }
}

// ── memory_delete ────────────────────────────────────────────────

#[derive(Deserialize)]
struct DeleteArgs {
    chunk_id: String,
    #[allow(dead_code)]
    #[serde(default)]
    stream: Option<String>,
}

async fn tool_delete(
    state: &Arc<AppState>,
    args: Value,
    stream_id: &str,
) -> Result<ToolResult, JsonRpcError> {
    let args: DeleteArgs =
        serde_json::from_value(args).map_err(|e| JsonRpcError::invalid_params(&e.to_string()))?;

    // Validate chunk ownership before deletion
    if let Ok(Some(chunk)) = state.store.get_chunk(&args.chunk_id) {
        if chunk.stream != stream_id {
            return Ok(ToolResult::error(
                "Access denied: this memory belongs to another stream.".to_string(),
            ));
        }
    }

    match crate::handlers::delete::delete_memory_fully(
        &state.store,
        &state.tantivy,
        &state.graph,
        &args.chunk_id,
    )
    .await
    {
        Ok(outcome) if !outcome.store_deleted => Ok(ToolResult::text(format!(
            "Memory {} not found.",
            args.chunk_id
        ))),
        Ok(outcome) if outcome.all_ok() => Ok(ToolResult::text(format!(
            "Deleted memory {}.",
            args.chunk_id
        ))),
        Ok(outcome) => {
            // Cycle/117 partial-success: chunk is soft-deleted but at least
            // one downstream step failed. Report so caller (MCP client) can
            // retry the delete or surface to the user.
            let failures: Vec<&str> = [
                ("store", outcome.store.is_err()),
                ("tantivy", outcome.tantivy.is_err()),
                ("embedding", outcome.embedding.is_err()),
                ("graph", outcome.graph.is_err()),
            ]
            .into_iter()
            .filter_map(|(name, failed)| if failed { Some(name) } else { None })
            .collect();
            Ok(ToolResult::text(format!(
                "Deleted memory {} (partial: failed steps = {:?}; retry recommended).",
                args.chunk_id, failures
            )))
        }
        Err(e) => Ok(ToolResult::error(format!("Delete failed: {}", e))),
    }
}

// ── memory_associate ────────────────────────────────────────────

#[derive(Deserialize)]
struct AssociateArgs {
    query: String,
    mechanisms: Option<Vec<String>>,
    count: Option<usize>,
    #[allow(dead_code)]
    #[serde(default)]
    stream: Option<String>,
}

async fn tool_associate(
    state: &Arc<AppState>,
    args: Value,
    stream_id: &str,
) -> Result<ToolResult, JsonRpcError> {
    let args: AssociateArgs =
        serde_json::from_value(args).map_err(|e| JsonRpcError::invalid_params(&e.to_string()))?;

    let req = handlers::types::AssociateRequest {
        query: args.query,
        stream_id: Some(stream_id.to_string()),
        mechanisms: args.mechanisms,
        count: args.count,
        hops: None,
    };

    // Internal synthetic auth for a nested handler call. The caller's real
    // auth was already enforced by the outer MCP request handler; this stub
    // is server-originated (user_id = None) so it carries Admin+Shared per
    // brief §G3 to avoid spurious role gate failures. stream_id is whatever
    // the outer caller's derived stream was (DEFAULT_STREAM_ID for Shared,
    // user.stream_id for Private — handed through verbatim).
    let auth = crate::auth::AuthContext::single_stream(
        stream_id.to_string(),
        loomem_core::storage::UserRole::Admin,
        crate::auth::KeyScope::Shared,
        None,
        true,
    );

    match handlers::search::associate_handler(
        State(state.clone()),
        axum::Extension(auth),
        Json(req),
    )
    .await
    {
        Ok(Json(resp)) => {
            if resp.associations.is_empty() {
                return Ok(ToolResult::text("No associations found. This may mean clustering hasn't run yet (trigger with memory_dream) or no serendipitous connections exist for this query."));
            }
            let mut text = format!(
                "Found {} associations (took {}ms):\n\n",
                resp.associations.len(),
                resp.took_ms
            );
            for (i, a) in resp.associations.iter().enumerate() {
                text.push_str(&format!(
                    "{}. [{}] (score: {:.3}) {}\n",
                    i + 1,
                    a.source_mechanism,
                    a.score,
                    a.content,
                ));
                if let Some(ref explanation) = a.explanation {
                    text.push_str(&format!("   {}\n", explanation));
                }
            }
            text.push_str("\nThese are serendipitous connections — related but not obvious. Use them to spark new insights.");
            Ok(ToolResult::text(text))
        }
        Err(e) => Ok(ToolResult::error(format!("Association failed: {:?}", e))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{AuthContext, KeyScope, StreamMembership};
    use loomem_core::storage::{UserRole, DEFAULT_STREAM_ID};
    use serde_json::json;

    /// Build an AuthContext with a single membership matching the default.
    /// Matches the pre-/32 ctx() helper's semantics for the gate tests.
    fn ctx(role: UserRole, scope: KeyScope) -> AuthContext {
        AuthContext::single_stream("s", role, scope, Some("u".into()), role.is_admin())
    }

    // ── /151 AC-1: tool_store event_date → valid_from routing ──

    /// AC-1 (/151): an extracted event date lands in both the ISO
    /// extraction_meta field and the chunk's `valid_from` (UTC midnight unix)
    /// — exactly the pair `tool_store` writes into the chunk literal.
    #[test]
    fn ac1_event_date_routing_maps_date_to_valid_from() {
        let date = chrono::NaiveDate::from_ymd_opt(1992, 12, 1).expect("valid date");
        let (iso, valid_from) = event_date_routing(Some(date), 1_750_000_000);
        assert_eq!(iso.as_deref(), Some("1992-12-01"));
        // 1992-12-01 00:00:00 UTC
        assert_eq!(valid_from, 723_168_000);
    }

    /// AC-1 (/151): no extracted date (timeless fact / LLM failure / gate
    /// off) → extraction_meta.event_date stays None and valid_from falls
    /// back to the ingest timestamp — byte-identical to pre-/151 behavior.
    #[test]
    fn ac1_event_date_routing_falls_back_to_timestamp() {
        let (iso, valid_from) = event_date_routing(None, 1_750_000_000);
        assert_eq!(iso, None);
        assert_eq!(valid_from, 1_750_000_000);
    }

    /// AC-1 (/151): pre-1970 dates cannot be represented in the u64
    /// valid_from — fall back to the ingest timestamp (the ISO meta field
    /// still records the date for provenance).
    #[test]
    fn ac1_event_date_routing_pre_1970_falls_back() {
        let date = chrono::NaiveDate::from_ymd_opt(1956, 6, 15).expect("valid date");
        let (iso, valid_from) = event_date_routing(Some(date), 1_750_000_000);
        assert_eq!(iso.as_deref(), Some("1956-06-15"));
        assert_eq!(valid_from, 1_750_000_000);
    }

    // AC-5 (/143): MCP tool_search content-type tag is just `[content_type]`
    // (band/source dropped — ADR-017 Amendment v2).
    #[test]
    fn ac5_content_type_tag_format() {
        assert_eq!(content_type_tag(Some("case_study")), " [case_study]");
        assert_eq!(content_type_tag(Some("other")), " [other]");
        // Unclassified chunk (no sidecar hit) → no tag.
        assert_eq!(content_type_tag(None), "");
    }

    /// Build an AuthContext with an explicit memberships list.
    /// First entry is the default stream (determines scalar fields).
    fn multi_membership_auth(
        default_stream_id: &str,
        entries: Vec<(&str, UserRole, KeyScope)>,
    ) -> AuthContext {
        let memberships: Vec<StreamMembership> = entries
            .iter()
            .map(|(sid, role, src)| StreamMembership {
                stream_id: (*sid).to_string(),
                role: *role,
                source: *src,
            })
            .collect();
        let (_, role, scope) = *entries.first().expect("at least one membership");
        AuthContext {
            stream_id: default_stream_id.to_string(),
            user_id: Some("test_user".into()),
            is_admin: role.is_admin(),
            role,
            scope,
            memberships,
        }
    }

    // ── Pre-/32 gate regressions (rewritten against new `gate_tool` API) ──

    #[test]
    fn writer_on_shared_can_store() {
        assert!(gate_tool("memory_store", UserRole::Writer, KeyScope::Shared).is_none());
    }

    #[test]
    fn reader_on_shared_cannot_store() {
        assert!(gate_tool("memory_store", UserRole::Reader, KeyScope::Shared).is_some());
    }

    #[test]
    fn reader_on_private_can_store() {
        // Owner rights on private — Reader role on their own stream still writes.
        assert!(gate_tool("memory_store", UserRole::Reader, KeyScope::Private).is_none());
    }

    #[test]
    fn writer_on_shared_cannot_delete() {
        assert!(gate_tool("memory_delete", UserRole::Writer, KeyScope::Shared).is_some());
    }

    #[test]
    fn admin_on_shared_can_delete() {
        assert!(gate_tool("memory_delete", UserRole::Admin, KeyScope::Shared).is_none());
    }

    #[test]
    fn reader_on_private_can_delete() {
        assert!(gate_tool("memory_delete", UserRole::Reader, KeyScope::Private).is_none());
    }

    #[test]
    fn read_ops_allowed_for_reader_on_shared() {
        assert!(gate_tool("memory_search", UserRole::Reader, KeyScope::Shared).is_none());
    }

    // ── AC-2.x — resolve_stream_for_call ──

    #[test]
    fn ac2_2_resolve_no_stream_returns_default() {
        let auth = ctx(UserRole::Writer, KeyScope::Private);
        let args = json!({ "content": "hello" });
        let r = resolve_stream_for_call(&args, "default_sid", &auth).expect("ok");
        assert_eq!(r.stream_id, "default_sid");
        assert_eq!(r.role, UserRole::Writer);
        assert_eq!(r.source, KeyScope::Private);
    }

    #[test]
    fn ac2_3_resolve_empty_stream_returns_default() {
        let auth = ctx(UserRole::Reader, KeyScope::Shared);
        let args = json!({ "content": "x", "stream": "" });
        let r = resolve_stream_for_call(&args, "default_sid", &auth).expect("ok");
        assert_eq!(r.stream_id, "default_sid");
        assert_eq!(r.role, UserRole::Reader);
        assert_eq!(r.source, KeyScope::Shared);
    }

    #[test]
    fn ac2_3b_resolve_null_stream_returns_default() {
        // JSON null should be treated identically to missing.
        let auth = ctx(UserRole::Reader, KeyScope::Shared);
        let args = json!({ "content": "x", "stream": Value::Null });
        let r = resolve_stream_for_call(&args, "default_sid", &auth).expect("ok");
        assert_eq!(r.stream_id, "default_sid");
    }

    #[test]
    fn ac2_4_resolve_explicit_matching_membership() {
        let auth = multi_membership_auth(
            "__user_u__",
            vec![
                ("__user_u__", UserRole::Admin, KeyScope::Private),
                (DEFAULT_STREAM_ID, UserRole::Writer, KeyScope::Shared),
            ],
        );
        let args = json!({ "content": "x", "stream": DEFAULT_STREAM_ID });
        let r = resolve_stream_for_call(&args, "__user_u__", &auth).expect("ok");
        assert_eq!(r.stream_id, DEFAULT_STREAM_ID);
        assert_eq!(r.role, UserRole::Writer);
        assert_eq!(r.source, KeyScope::Shared);
    }

    #[test]
    fn ac2_5_resolve_explicit_no_membership_returns_access_denied() {
        let auth = ctx(UserRole::Writer, KeyScope::Shared);
        let args = json!({ "content": "x", "stream": "__user_someone_else__" });
        let deny = resolve_stream_for_call(&args, "default_sid", &auth).expect_err("should deny");
        assert_eq!(deny.is_error, Some(true));
        let msg = &deny.content[0].text;
        assert!(
            msg.contains("Access denied") && msg.contains("__user_someone_else__"),
            "denial text unexpected: {msg}"
        );
    }

    #[test]
    fn ac2_5b_resolve_denial_does_not_leak_other_stream_existence() {
        // Two users X and Y both exist; caller is X. Y's stream id is
        // "__user_y__". Caller tries it → the denial must NOT disclose whether
        // Y exists or not (it just says "no membership", same as if sid was
        // random garbage). Use two callers to compare denial text.
        let auth_caller = ctx(UserRole::Writer, KeyScope::Shared);
        let args_real = json!({ "stream": "__user_y__", "content": "x" });
        let args_bogus = json!({ "stream": "__user_does_not_exist_anywhere__", "content": "x" });
        let d1 = resolve_stream_for_call(&args_real, "default_sid", &auth_caller)
            .expect_err("deny real");
        let d2 = resolve_stream_for_call(&args_bogus, "default_sid", &auth_caller)
            .expect_err("deny bogus");
        // Both denials share the same template; the only variable is the
        // echoed id. Nothing else about storage existence leaks.
        assert!(d1.content[0]
            .text
            .starts_with("Access denied: no membership on stream "));
        assert!(d2.content[0]
            .text
            .starts_with("Access denied: no membership on stream "));
    }

    #[test]
    fn ac2_6_resolve_explicit_same_as_default() {
        let auth = multi_membership_auth(
            "__user_u__",
            vec![("__user_u__", UserRole::Admin, KeyScope::Private)],
        );
        let args = json!({ "stream": "__user_u__", "content": "x" });
        let r = resolve_stream_for_call(&args, "__user_u__", &auth).expect("ok");
        assert_eq!(r.stream_id, "__user_u__");
        assert_eq!(r.role, UserRole::Admin);
        assert_eq!(r.source, KeyScope::Private);
    }

    // ── AC-3.x — gate_tool ──

    #[test]
    fn ac3_1_gate_private_membership_no_gate_for_all_tools() {
        // Private source → None regardless of role, for EVERY tool name the
        // dispatcher recognizes. Enumerate to regression-guard future
        // additions that might forget the Private early-return.
        let tools = [
            "memory_store",
            "memory_search",
            "memory_context",
            "memory_profile",
            "memory_status",
            "memory_reflect",
            "memory_graph",
            "memory_ingest",
            "memory_dream",
            "memory_history",
            "memory_delete",
            "memory_associate",
            "memory_namespaces",
        ];
        for tool in tools {
            for role in [UserRole::Reader, UserRole::Writer, UserRole::Admin] {
                assert!(
                    gate_tool(tool, role, KeyScope::Private).is_none(),
                    "Private+{role:?} should not gate {tool}"
                );
            }
        }
    }

    #[test]
    fn ac3_2_gate_shared_reader_denies_store() {
        let out = gate_tool("memory_store", UserRole::Reader, KeyScope::Shared).expect("deny");
        assert_eq!(out.is_error, Some(true));
        assert!(out.content[0].text.contains("Read-only access"));
    }

    #[test]
    fn ac3_3_gate_shared_writer_allows_store() {
        assert!(gate_tool("memory_store", UserRole::Writer, KeyScope::Shared).is_none());
    }

    #[test]
    fn ac3_4_gate_shared_writer_denies_delete() {
        let out = gate_tool("memory_delete", UserRole::Writer, KeyScope::Shared).expect("deny");
        assert!(out.content[0].text.contains("Admin-only"));
    }

    #[test]
    fn ac3_5_gate_shared_admin_allows_delete() {
        assert!(gate_tool("memory_delete", UserRole::Admin, KeyScope::Shared).is_none());
    }

    // AC-3.6 — gate_tool_by_role_and_scope removed. Enforced by grep in CI +
    // critic; at test-level, the symbol is simply not in scope. This test
    // guards that the new `gate_tool` is the only gate function exported.
    #[test]
    fn ac3_6_gate_tool_is_sole_gate_fn() {
        // Compilation of this test calling gate_tool is the guard.
        let _ = gate_tool("memory_store", UserRole::Writer, KeyScope::Shared);
    }

    // AC-3.7 — /31 gotcha regression
    // User with memberships = [Private+Admin on own, Shared+Admin on acme]
    // → memory_delete on OWN stream: allowed (Private early-return),
    //   memory_delete on SHARED stream: allowed (Shared+Admin passes gate).
    // User with memberships = [Shared+Writer only] → memory_delete on shared: denied.
    #[test]
    fn ac3_7_private_admin_vs_shared_admin_vs_shared_writer_on_delete() {
        // User A: owner of own private + admin on shared
        let user_a_on_own = gate_tool("memory_delete", UserRole::Admin, KeyScope::Private);
        assert!(user_a_on_own.is_none(), "Private+Admin should not gate");
        let user_a_on_shared = gate_tool("memory_delete", UserRole::Admin, KeyScope::Shared);
        assert!(user_a_on_shared.is_none(), "Shared+Admin can delete shared");

        // User B: writer on shared (no private)
        let user_b_on_shared = gate_tool("memory_delete", UserRole::Writer, KeyScope::Shared);
        assert!(
            user_b_on_shared.is_some(),
            "Shared+Writer must NOT be able to delete"
        );
    }

    // AC-3.7 extra — Private+Reader (pathological: user role downgraded to
    // Reader globally but still owner of own stream) must retain full rights.
    #[test]
    fn ac3_7b_private_reader_preserves_owner_rights() {
        assert!(gate_tool("memory_delete", UserRole::Reader, KeyScope::Private).is_none());
        assert!(gate_tool("memory_dream", UserRole::Reader, KeyScope::Private).is_none());
        assert!(gate_tool("memory_store", UserRole::Reader, KeyScope::Private).is_none());
    }

    // ── AC-5.x — tool_namespaces output (access label rules) ──
    //
    // tool_namespaces hits state.config (AppState), which makes full
    // end-to-end tests integration-tier. The access-label logic is the
    // behaviorally-important part; it's factored into `access_label` and
    // unit-tested in isolation here.

    #[test]
    fn ac5_3_access_label_private_is_owner_regardless_of_role() {
        assert_eq!(access_label(KeyScope::Private, UserRole::Reader), "owner");
        assert_eq!(access_label(KeyScope::Private, UserRole::Writer), "owner");
        assert_eq!(access_label(KeyScope::Private, UserRole::Admin), "owner");
    }

    #[test]
    fn ac5_4_access_label_shared_role_mapping() {
        assert_eq!(access_label(KeyScope::Shared, UserRole::Admin), "admin");
        assert_eq!(access_label(KeyScope::Shared, UserRole::Writer), "write");
        assert_eq!(access_label(KeyScope::Shared, UserRole::Reader), "read");
    }

    // ── AC-6.x — backward compat ──

    #[test]
    fn ac6_1_pre32_json_deserializes_and_resolves_to_default() {
        // Pre-/32 shape: StoreArgs without `stream` field.
        let args = json!({ "content": "pre-32", "source": null });
        let _: StoreArgs = serde_json::from_value(args.clone()).expect("deserialize ok");
        let auth = ctx(UserRole::Writer, KeyScope::Private);
        let r = resolve_stream_for_call(&args, "legacy_default", &auth).expect("resolve ok");
        assert_eq!(r.stream_id, "legacy_default");
    }

    #[test]
    fn ac6_1b_pre32_various_args_structs_deserialize_without_stream() {
        // Round-trip each stream-accepting args struct without a `stream` key.
        let _: SearchArgs = serde_json::from_value(json!({ "query": "q" })).unwrap();
        let _: ContextArgs = serde_json::from_value(json!({})).unwrap();
        let _: ProfileArgs = serde_json::from_value(json!({})).unwrap();
        let _: StatusArgs = serde_json::from_value(json!({})).unwrap();
        let _: ReflectArgs = serde_json::from_value(json!({})).unwrap();
        let _: GraphArgs = serde_json::from_value(json!({ "entity": "acme" })).unwrap();
        let _: IngestArgs = serde_json::from_value(json!({ "content": "x" })).unwrap();
        let _: DreamArgs = serde_json::from_value(json!({})).unwrap();
        let _: HistoryArgs = serde_json::from_value(json!({ "chunk_id": "c1" })).unwrap();
        let _: DeleteArgs = serde_json::from_value(json!({ "chunk_id": "c1" })).unwrap();
        let _: AssociateArgs = serde_json::from_value(json!({ "query": "q" })).unwrap();
    }

    // ── AC-6.3 (per addendum) — tools/list schema emission ──

    #[test]
    fn ac6_3_tool_definitions_has_stream_in_twelve_tools_and_not_in_namespaces() {
        let defs = crate::mcp::tools::tool_definitions();
        // 13 memory tools + 1 feedback tool (/113) = 14.
        let expected_total = 13 + 1;
        assert_eq!(
            defs.len(),
            expected_total,
            "expected {expected_total} tool definitions"
        );

        let mut with_stream = 0usize;
        let mut namespaces_without_stream = false;

        for def in &defs {
            let name = def.get("name").and_then(Value::as_str).expect("name");
            let properties = def
                .get("inputSchema")
                .and_then(|s| s.get("properties"))
                .and_then(Value::as_object)
                .expect("properties object");
            let has_stream = properties.contains_key("stream");

            if name == "memory_namespaces" {
                assert!(
                    !has_stream,
                    "memory_namespaces must NOT have 'stream' property"
                );
                namespaces_without_stream = true;
            } else if name == "memory_feedback" {
                // Cycle/113: memory_feedback uses session stream_id, no per-call
                // stream arg. Exempt from the stream-property assertion.
                assert!(
                    !has_stream,
                    "memory_feedback must NOT have 'stream' property (uses session stream)"
                );
            } else {
                assert!(
                    has_stream,
                    "tool {name} is missing 'stream' property in inputSchema.properties"
                );
                // Verify stream shape per addendum §4 point 1.
                let stream_prop = properties.get("stream").unwrap();
                assert_eq!(
                    stream_prop.get("type").and_then(Value::as_str),
                    Some("string")
                );
                let desc = stream_prop
                    .get("description")
                    .and_then(Value::as_str)
                    .expect("description on stream property");
                assert!(
                    desc.contains("Optional stream_id") && desc.contains("memory_namespaces"),
                    "stream description not verbatim for {name}: {desc}"
                );
                // Verify stream is NOT in required[].
                if let Some(required) = def
                    .get("inputSchema")
                    .and_then(|s| s.get("required"))
                    .and_then(Value::as_array)
                {
                    assert!(
                        !required.iter().any(|v| v.as_str() == Some("stream")),
                        "tool {name} erroneously lists 'stream' in required"
                    );
                }
                with_stream += 1;
            }
        }

        assert_eq!(
            with_stream, 12,
            "exactly 12 memory tools must have 'stream' property"
        );
        assert!(
            namespaces_without_stream,
            "memory_namespaces not found in catalog"
        );
    }

    // ── AC-6.3 extra: verbatim consistency of the `stream` description ──

    #[test]
    fn ac6_3_stream_description_is_identical_across_twelve_tools() {
        let defs = crate::mcp::tools::tool_definitions();
        let descriptions: Vec<String> = defs
            .iter()
            .filter_map(|d| {
                let name = d.get("name").and_then(Value::as_str)?;
                if name == "memory_namespaces" {
                    return None;
                }
                d.get("inputSchema")?
                    .get("properties")?
                    .get("stream")?
                    .get("description")?
                    .as_str()
                    .map(String::from)
            })
            .collect();
        assert_eq!(descriptions.len(), 12);
        let first = &descriptions[0];
        for d in &descriptions[1..] {
            assert_eq!(d, first, "stream description must be verbatim across tools");
        }
    }

    // ── cycle/33 — per-project RBAC via source=Shared + role mapping ──

    // AC-4.6 — memory_associate gate amendment (deliberate behavior change
    // per brief §3.4). Pre-/33: Reader+Shared+memory_associate was allowed
    // (fell through `_ => None`). Post-/33: denied like memory_store.
    //
    // NOTE: there is no pre-/32 dispatcher test asserting the OLD behavior
    // (Reader+Shared+associate allowed), so this amendment does not break a
    // locked-in assertion. The `_ => None` fall-through was implicit in the
    // `read_ops_allowed_for_reader_on_shared` test (which uses memory_search,
    // not memory_associate), so that test stays green.
    #[test]
    fn ac4_6_memory_associate_denied_for_shared_reader_post_33() {
        let out = gate_tool("memory_associate", UserRole::Reader, KeyScope::Shared)
            .expect("reader+shared+associate must deny post-/33 §3.4");
        assert!(out.content[0].text.contains("Read-only access"));
    }

    #[test]
    fn ac4_6_memory_associate_allowed_for_shared_writer() {
        assert!(gate_tool("memory_associate", UserRole::Writer, KeyScope::Shared).is_none());
    }

    #[test]
    fn ac4_6_memory_associate_allowed_for_shared_admin() {
        assert!(gate_tool("memory_associate", UserRole::Admin, KeyScope::Shared).is_none());
    }

    // AC-4.1 — project Owner (stored as source=Shared + role=Admin in
    // compute_memberships via project_role_to_user_role) gets full rights
    // on their project, including delete + dream + associate.
    #[test]
    fn ac4_1_project_owner_full_rights_on_own_project() {
        // Per /33 §3.4, Owner maps to UserRole::Admin with source=Shared;
        // Shared+Admin gate allows delete, dream, and associate.
        for tool in [
            "memory_store",
            "memory_ingest",
            "memory_delete",
            "memory_dream",
            "memory_associate",
        ] {
            assert!(
                gate_tool(tool, UserRole::Admin, KeyScope::Shared).is_none(),
                "Owner (Shared+Admin) should allow {tool}"
            );
        }
    }

    // AC-4.2 — project Writer: write+ingest+associate allowed;
    // delete+dream denied (Admin-only per §D5).
    #[test]
    fn ac4_2_project_writer_gated_like_shared_writer() {
        assert!(gate_tool("memory_store", UserRole::Writer, KeyScope::Shared).is_none());
        assert!(gate_tool("memory_ingest", UserRole::Writer, KeyScope::Shared).is_none());
        assert!(gate_tool("memory_associate", UserRole::Writer, KeyScope::Shared).is_none());
        assert!(gate_tool("memory_delete", UserRole::Writer, KeyScope::Shared).is_some());
        assert!(gate_tool("memory_dream", UserRole::Writer, KeyScope::Shared).is_some());
    }

    // AC-4.3 — project Reader: all writes + associate denied; reads allowed.
    #[test]
    fn ac4_3_project_reader_gated_like_shared_reader() {
        for tool in [
            "memory_store",
            "memory_ingest",
            "memory_associate",
            "memory_delete",
            "memory_dream",
        ] {
            assert!(
                gate_tool(tool, UserRole::Reader, KeyScope::Shared).is_some(),
                "Reader on project must deny {tool}"
            );
        }
        for tool in [
            "memory_search",
            "memory_context",
            "memory_profile",
            "memory_status",
            "memory_reflect",
            "memory_graph",
            "memory_history",
            "memory_namespaces",
        ] {
            assert!(
                gate_tool(tool, UserRole::Reader, KeyScope::Shared).is_none(),
                "Reader on project must allow read-only {tool}"
            );
        }
    }

    // AC-4.5 — Global admin (Shared+Admin from shared-key auth) WITHOUT a
    // project membership is not auto-granted project rights. Protection lives
    // at the resolver layer (`resolve_stream_for_call`): if the caller has
    // no membership matching the explicit `stream` arg, resolver returns
    // "Access denied" BEFORE the gate runs. Gate alone cannot distinguish
    // "Admin on shared" from "Admin on project" — resolver enforces the
    // membership boundary.
    #[test]
    fn ac4_5_global_admin_without_project_membership_denied_at_resolver() {
        use crate::auth::StreamMembership;

        // Global admin via shared key: memberships = [DEFAULT_STREAM_ID only].
        let auth = AuthContext {
            stream_id: DEFAULT_STREAM_ID.to_string(),
            user_id: Some("global_admin".into()),
            is_admin: true,
            role: UserRole::Admin,
            scope: KeyScope::Shared,
            memberships: vec![StreamMembership {
                stream_id: DEFAULT_STREAM_ID.to_string(),
                role: UserRole::Admin,
                source: KeyScope::Shared,
            }],
        };
        let args = json!({ "stream": "__project_foreign__", "content": "x" });
        let deny = resolve_stream_for_call(&args, DEFAULT_STREAM_ID, &auth)
            .expect_err("global admin without project membership must be denied at resolver");
        assert!(deny.content[0].text.contains("Access denied"));
    }

    // ── /141: memory_namespaces aliases + types (ADR-016) ────────────────────

    /// AC-1 (/141): `type_label` derives correctly from `classify_stream` for
    /// each kind, independent of any stored alias (empty config + empty store).
    #[tokio::test]
    async fn namespaces_type_labels_match_kind() {
        let (_r, state) = crate::tests::make_test_app();
        let ns = state.config.namespaces.clone();
        let cases = [
            ("__user_abc__", "user_private"),
            (DEFAULT_STREAM_ID, "user_private"),
            ("__shared_team__", "organization_shared"),
            ("__project_xyz__", "project"),
        ];
        for (sid, expected) in cases {
            let (_alias, label) = alias_and_type(classify_stream(sid), sid, &ns);
            assert_eq!(label, expected, "type label for {sid}");
        }
    }

    /// AC-3 (/141): the default stream gets the generic private alias +
    /// `type: user_private`.
    #[tokio::test]
    async fn namespaces_default_alias_and_type() {
        let (_r, state) = crate::tests::make_test_app();
        let auth = auth_non_admin_with_memberships(vec![DEFAULT_STREAM_ID]);
        let result = tool_namespaces(&state, DEFAULT_STREAM_ID, &auth.memberships)
            .await
            .expect("tool_namespaces Ok");
        let text = &result.content[0].text;
        assert!(
            text.contains("type: user_private"),
            "default stream type label: {text}"
        );
        assert!(
            text.contains("Your private memory"),
            "default stream alias: {text}"
        );
    }

    /// AC-4 (/141) PRIVACY GUARD: a private `__user_<uuid>` stream's line never
    /// contains a person/email — only a generic alias + `type: user_private`.
    #[tokio::test]
    async fn namespaces_private_no_pii_and_type() {
        let (_r, state) = crate::tests::make_test_app();
        let auth =
            auth_non_admin_with_memberships(vec!["__user_2f9d4c7a-0000-4000-8000-000000000000__"]);
        let result = tool_namespaces(&state, "__user_default__", &auth.memberships)
            .await
            .expect("tool_namespaces Ok");
        let text = &result.content[0].text;
        assert!(
            text.contains("type: user_private"),
            "private type label: {text}"
        );
        assert!(
            text.contains("Your private memory"),
            "private generic alias: {text}"
        );
        assert!(
            !text.contains('@'),
            "private line must not leak an email: {text}"
        );
    }

    /// AC-5 (/141): backward-compatible line shape — still
    /// `- <alias> (stream_id: <id>, ...)`, with `type:` added inline.
    #[tokio::test]
    async fn namespaces_backward_compat_line_shape() {
        let (_r, state) = crate::tests::make_test_app();
        let auth = auth_non_admin_with_memberships(vec![DEFAULT_STREAM_ID]);
        let result = tool_namespaces(&state, DEFAULT_STREAM_ID, &auth.memberships)
            .await
            .expect("tool_namespaces Ok");
        let text = &result.content[0].text;
        let line = text
            .lines()
            .find(|l| l.contains(DEFAULT_STREAM_ID))
            .expect("stream line present");
        assert!(line.starts_with("- "), "line prefix: {line}");
        assert!(line.contains("(stream_id: "), "stream_id field: {line}");
        assert!(line.contains(", type: "), "type field added: {line}");
        assert!(line.contains(", access: "), "access field retained: {line}");
        assert!(
            line.contains(", default"),
            "default marker retained: {line}"
        );
    }

    /// Build a non-admin AuthContext with the given memberships.
    fn auth_non_admin_with_memberships(memberships: Vec<&str>) -> AuthContext {
        AuthContext {
            stream_id: memberships
                .first()
                .unwrap_or(&"__user_default__")
                .to_string(),
            user_id: Some("u".into()),
            is_admin: false,
            role: UserRole::Reader,
            scope: KeyScope::Private,
            memberships: memberships
                .into_iter()
                .map(|s| StreamMembership {
                    stream_id: s.to_string(),
                    role: UserRole::Reader,
                    source: KeyScope::Private,
                })
                .collect(),
        }
    }

    // ── /157 S1 — AC-1/AC-2: loud extraction failures in memory_ingest ──

    /// Failing stub: simulates OpenAI 429 quota exhaustion (incident A).
    struct Quota429;

    impl loomem_core::memory_extractor::ExtractionChat for Quota429 {
        async fn chat(
            &self,
            _request_body: &serde_json::Value,
        ) -> anyhow::Result<loomem_core::memory_extractor::ChatHttpReply> {
            Ok(loomem_core::memory_extractor::ChatHttpReply {
                status: 429,
                body: r#"{"error":{"code":"insufficient_quota","message":"You exceeded your current quota"}}"#.to_string(),
            })
        }
    }

    /// Success stub: valid chat envelope carrying zero facts.
    struct EmptyFacts;

    impl loomem_core::memory_extractor::ExtractionChat for EmptyFacts {
        async fn chat(
            &self,
            _request_body: &serde_json::Value,
        ) -> anyhow::Result<loomem_core::memory_extractor::ChatHttpReply> {
            Ok(loomem_core::memory_extractor::ChatHttpReply {
                status: 200,
                body: serde_json::json!({
                    "choices": [{"message": {"content": "{\"facts\": []}"}}]
                })
                .to_string(),
            })
        }
    }

    /// AC-1 (integration): a simulated 429 through the real `tool_ingest`
    /// extraction branch → explicit extraction error, NOT "Extracted 0 facts".
    #[tokio::test]
    async fn ac1_ingest_429_is_loud_error_not_extracted_zero() {
        let (_app, state) = crate::tests::make_test_app();
        let result = tool_ingest_extract(
            &Quota429,
            &state,
            "transcript text",
            "2026-06-11",
            "test_stream",
            None,
        )
        .await
        .expect("dispatcher returns a ToolResult, not a JsonRpcError");
        assert_eq!(result.is_error, Some(true));
        let text = &result.content[0].text;
        assert!(text.starts_with("Extraction failed (429):"), "got: {text}");
        assert!(text.contains("insufficient_quota"), "got: {text}");
        assert!(text.ends_with("No facts stored."), "got: {text}");
        assert!(!text.contains("Extracted 0 facts"), "got: {text}");
    }

    /// AC-2 (integration): a genuine zero-fact success through the same
    /// branch reads exactly as before /157 — no error, legacy message.
    #[tokio::test]
    async fn ac2_ingest_zero_facts_reads_as_before() {
        let (_app, state) = crate::tests::make_test_app();
        let result = tool_ingest_extract(
            &EmptyFacts,
            &state,
            "smalltalk only",
            "2026-06-11",
            "test_stream",
            None,
        )
        .await
        .expect("dispatcher returns a ToolResult, not a JsonRpcError");
        assert_ne!(result.is_error, Some(true));
        assert_eq!(
            result.content[0].text,
            "Extracted 0 facts from conversation, stored 0, skipped 0."
        );
    }

    /// AC-2 (unit): zero facts + zero failures formats byte-identically to
    /// the pre-/157 message.
    #[test]
    fn ac2_zero_facts_message_unchanged() {
        let outcome = loomem_core::memory_extractor::ExtractionOutcome::default();
        assert_eq!(
            ingest_summary(&outcome, 0, 0),
            "Extracted 0 facts from conversation, stored 0, skipped 0."
        );
    }

    /// /157 S1: partial success appends a warning with count + first error.
    #[test]
    fn partial_failure_warning_appended() {
        let outcome = loomem_core::memory_extractor::ExtractionOutcome {
            facts: Vec::new(),
            failures: vec![loomem_core::memory_extractor::ChunkFailure {
                chunk_index: 1,
                status: Some(429),
                reason: "quota".to_string(),
            }],
        };
        let msg = ingest_summary(&outcome, 0, 0);
        assert!(msg.starts_with("Extracted 0 facts"), "got: {msg}");
        assert!(
            msg.contains("Warning: 1 extraction chunk(s) failed"),
            "got: {msg}"
        );
        assert!(msg.contains("(429): quota"), "got: {msg}");
    }

    /// AC-6 (/157): memory_status surfaces the undecodable-chunks counter and
    /// the windowed LLM failure counts.
    #[tokio::test]
    async fn ac6_memory_status_reports_backlog_and_llm_failures() {
        let (_app, state) = crate::tests::make_test_app();
        let result = tool_status(&state, serde_json::json!({}), "test_stream")
            .await
            .expect("ToolResult");
        let text = &result.content[0].text;
        assert!(
            text.contains("Undecodable chunks (last full scan):"),
            "got: {text}"
        );
        assert!(
            text.contains("LLM failures (last 60m): extraction="),
            "got: {text}"
        );
    }
}

// ── Cycle /113: memory_feedback ───────────────────────────────────

async fn tool_feedback(
    state: &Arc<AppState>,
    args: Value,
    stream_id: &str,
    auth: &AuthContext,
) -> Result<ToolResult, JsonRpcError> {
    #[derive(Deserialize)]
    struct FeedbackArgs {
        chunk_id: String,
        usefulness: u8,
        harmful: bool,
        justification: String,
        model_version: String,
        #[serde(default = "default_prompt_version")]
        prompt_version: String,
        #[serde(default)]
        trajectory_id: Option<String>,
    }
    fn default_prompt_version() -> String {
        "loomem-feedback-v1".to_string()
    }

    let args: FeedbackArgs =
        serde_json::from_value(args).map_err(|e| JsonRpcError::invalid_params(&e.to_string()))?;

    let cfg = &state.config.feedback;
    if !cfg.enabled {
        return Ok(ToolResult::error(
            "feedback endpoint disabled in config".to_string(),
        ));
    }

    let svc = FeedbackService::new(&state.store, cfg);

    if let Err(e) = svc.validate_rating(args.usefulness, args.harmful, &args.justification) {
        return Ok(ToolResult::error(format!("validation failed: {e}")));
    }

    let now_ms = chrono::Utc::now().timestamp_millis();
    let event_id = uuid::Uuid::new_v4().to_string();
    let agent_id = auth
        .user_id
        .clone()
        .unwrap_or_else(|| stream_id.to_string());

    let outcome = svc
        .apply_rating(ApplyRatingArgs {
            chunk_id: &args.chunk_id,
            usefulness: args.usefulness,
            harmful: args.harmful,
            justification: &args.justification,
            caller_stream: stream_id,
            caller_is_admin: auth.is_admin,
            agent_id: &agent_id,
            model_version: &args.model_version,
            prompt_version: &args.prompt_version,
            trajectory_id: args.trajectory_id.as_deref(),
            now_unix_ms: now_ms,
            event_id: &event_id,
        })
        .map_err(|e| JsonRpcError::internal(&format!("apply_rating: {e}")))?;

    let payload = match outcome {
        RatingOutcome::Accepted => serde_json::json!({
            "ok": true,
            "accepted": 1,
            "rejected": []
        }),
        RatingOutcome::Rejected { chunk_id, reason } => serde_json::json!({
            "ok": true,
            "accepted": 0,
            "rejected": [{
                "chunk_id": chunk_id,
                "reason": reason
            }]
        }),
    };
    Ok(ToolResult::text(
        serde_json::to_string_pretty(&payload).unwrap_or_default(),
    ))
}
