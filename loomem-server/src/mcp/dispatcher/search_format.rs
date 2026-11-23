//! `memory_search` result-line rendering. Extracted verbatim from
//! `tool_search` so the MCP text levers (predecessor lines, group headers)
//! can grow here; the header, footer and token budget stay in the
//! dispatcher. One result → one line, the id always last.
use crate::handlers::types::SearchResult;
use loomem_core::storage::RocksDbStore;
use serde_json::Value;

/// Render every hit, numbered from 1, in order.
pub(super) fn render_lines(store: &RocksDbStore, hits: &[SearchResult]) -> String {
    let mut text = String::new();
    for (i, r) in hits.iter().enumerate() {
        text.push_str(&render_line(store, i, r));
    }
    text
}

/// One `memory_search` line for the hit at zero-based `index`.
pub(super) fn render_line(store: &RocksDbStore, index: usize, r: &SearchResult) -> String {
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
