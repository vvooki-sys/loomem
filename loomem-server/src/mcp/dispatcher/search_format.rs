//! `memory_search` result-line rendering. Extracted verbatim from
//! `tool_search` so the MCP text levers (predecessor lines, group headers)
//! can grow here; the header, footer and token budget stay in the
//! dispatcher. One result → one line, the id always last; with
//! `[mcp] search_include_predecessor` on, an updated hit may be followed by
//! one `↳ previously` line carrying the version it superseded.
use crate::handlers::types::SearchResult;
use crate::AppState;
use loomem_core::storage::RocksDbStore;
use serde_json::Value;
use std::collections::{HashMap, HashSet};

#[cfg(test)]
mod predecessor_tests;

/// The direct predecessor (`supersedes_id`, one hop) of an updated hit, as
/// rendered under it: its date tag and its content capped at
/// `[mcp] predecessor_max_chars` characters.
pub(super) struct Predecessor {
    pub(super) ts: String,
    pub(super) text: String,
}

/// Predecessors keyed by hit id, or none at all when the lever is off or the
/// query is an aggregation (enumerations must not gain extra lines). Runs
/// before the token budget so the predecessor text is charged to its hit.
pub(super) fn predecessors_for(
    state: &AppState,
    stream_id: &str,
    hits: &[SearchResult],
    is_aggregation: bool,
) -> HashMap<String, Predecessor> {
    let mcp = &state.config.mcp;
    if !mcp.search_include_predecessor || is_aggregation {
        return HashMap::new();
    }
    predecessors(&state.store, stream_id, hits, mcp.predecessor_max_chars)
}

/// For each hit whose chunk supersedes another, load that predecessor in the
/// same stream. Skipped when it is soft-deleted, in another stream, missing,
/// or already one of the hits (e.g. `include_superseded = true`), so no
/// memory is shown twice.
pub(super) fn predecessors(
    store: &RocksDbStore,
    stream_id: &str,
    hits: &[SearchResult],
    max_chars: usize,
) -> HashMap<String, Predecessor> {
    let hit_ids: HashSet<&str> = hits.iter().map(|r| r.id.as_str()).collect();
    let mut out = HashMap::new();
    for r in hits {
        let Some(pid) = store
            .get_chunk(&r.id)
            .ok()
            .flatten()
            .and_then(|c| c.supersedes_id)
        else {
            continue;
        };
        if hit_ids.contains(pid.as_str()) {
            continue;
        }
        let Some(pred) = store.get_chunk_scoped(&pid, stream_id).ok().flatten() else {
            continue;
        };
        if pred.deleted_at.is_some() {
            continue;
        }
        let timestamp = i64::try_from(pred.timestamp).ok();
        let ts = crate::handlers::ingest_date::date_prefix(Some(&pred), timestamp);
        let text = cap_chars(&pred.content, max_chars);
        out.insert(r.id.clone(), Predecessor { ts, text });
    }
    out
}

/// `text` cut to at most `max_chars` characters, with `…` appended when cut.
pub(super) fn cap_chars(text: &str, max_chars: usize) -> String {
    match text.char_indices().nth(max_chars) {
        Some((end, _)) => format!("{}…", &text[..end]),
        None => text.to_string(),
    }
}

/// Estimated token cost of one rendered hit for `max_context_tokens`: its
/// content plus per-line overhead, and the same again for its predecessor
/// line when one is rendered. Equals the pre-lever cost when `preds` is empty.
pub(super) fn token_cost(r: &SearchResult, preds: &HashMap<String, Predecessor>) -> usize {
    let line = |text: &str| {
        loomem_core::llm_ner::estimate_tokens(text) + super::RESULT_FORMAT_TOKEN_OVERHEAD
    };
    line(&r.content) + preds.get(&r.id).map_or(0, |p| line(&p.text))
}

/// Render every hit, numbered from 1, in order, each followed by its
/// predecessor line when `preds` holds one for it.
pub(super) fn render_lines(
    store: &RocksDbStore,
    hits: &[SearchResult],
    preds: &HashMap<String, Predecessor>,
) -> String {
    let mut text = String::new();
    for (i, r) in hits.iter().enumerate() {
        text.push_str(&render_line(store, i, r, preds.get(&r.id)));
    }
    text
}

/// One `memory_search` line for the hit at zero-based `index`, plus the
/// `↳ previously` line when a predecessor is given.
pub(super) fn render_line(
    store: &RocksDbStore,
    index: usize,
    r: &SearchResult,
    predecessor: Option<&Predecessor>,
) -> String {
    let mut line = render_hit_line(store, index, r);
    if let Some(p) = predecessor {
        line.push_str(&format!("   ↳ previously [{}]: {}\n", p.ts, p.text));
    }
    line
}

fn render_hit_line(store: &RocksDbStore, index: usize, r: &SearchResult) -> String {
    let chunk_opt = store.get_chunk(&r.id).ok().flatten();
    let timestamp = r
        .metadata
        .as_ref()
        .and_then(|meta| meta.get("timestamp"))
        .and_then(Value::as_i64);
    let ts = crate::handlers::ingest_date::date_prefix(chunk_opt.as_ref(), timestamp);

    let supersedes_note = chunk_opt
        .as_ref()
        .and_then(|c| c.supersedes_id.as_ref())
        .map(|_| " [UPDATED — supersedes older version]")
        .unwrap_or("");

    // Cycle/142 + /143: content-type tag `[content_type]`, e.g.
    // `[case_study]`. Present only when the result was classified
    // (sidecar hit). Band/source dropped in /143 (ADR-017 Amd v2).
    let content_type_tag = content_type_tag(r.content_type.as_deref());

    // Cycle /001 (MemIR) observability: surface the trust tier and the
    // applied trust×provenance multiplier so the ranking effect is
    // visible (black-box testing + demos). None trust_level == "a1".
    // A `[trust:?]` sentinel marks a result whose chunk could not be
    // loaded (read error / index-vs-store desync) so it is not
    // confused with a chunk that legitimately has no trust metadata.
    let trust_tag = chunk_opt
        .as_ref()
        .map(|c| {
            format!(
                " [trust:{} ×{:.2}]",
                c.trust_level.as_deref().unwrap_or("a1"),
                loomem_core::hybrid_search::trust_provenance_multiplier(c)
            )
        })
        .unwrap_or_else(|| " [trust:?]".to_string());

    // Emit the chunk_id on each result line so callers can recover
    // it after a search alone — needed for memory_delete,
    // memory_history and memory_feedback, which all key on
    // chunk_id. Previously only memory_store/_ingest surfaced ids,
    // forcing callers to capture them at write time. The evidence
    // tag (`[search] evidence_fields`) sits before `(score:` so the
    // line still ends with the id; empty when the flag is off.
    format!(
        "{}. [{}]{}{} {}{}{} (score: {:.2}) (id: {})\n",
        index + 1,
        ts,
        content_type_tag,
        trust_tag,
        r.content,
        supersedes_note,
        crate::evidence::hit_text_suffix(r.evidence.as_ref()),
        r.score,
        r.id
    )
}

/// /142 + /143 (ADR-017): format the content-type tag ` [content_type]` for an
/// MCP result line, e.g. ` [case_study]`. Empty when the result carries no
/// classification (no sidecar hit). Since /143 the band/source are dropped from
/// the tag — `other` already signals uncertainty and source is always `llm`.
pub(super) fn content_type_tag(content_type: Option<&str>) -> String {
    match content_type {
        Some(ct) => format!(" [{ct}]"),
        None => String::new(),
    }
}
