//! `memory_search` group headers (`[mcp] search_group_by`). A `## <key>`
//! header goes before a hit whenever its key differs from the previous hit's.
//! Hits keep their ranked order and numbering — nothing is re-sorted — so a
//! group that is not contiguous gets its header again. With the lever off, or
//! for an aggregation query, the text is exactly `search_format::render_lines`.
//!
//! Headers are not charged to `max_context_tokens`: the budget trims hits
//! before rendering and counts hit (and predecessor) text only. Worst case is
//! one header per hit, so at most `[mcp] search_max_top_k` headers (20 as
//! shipped; aggregations get none). A header label is cut to
//! `MAX_HEADER_CHARS` characters, so one header line is at most ~87 bytes of
//! ASCII (~22 tokens by `estimate_tokens`, ~440 for 20 hits) and at most
//! ~300 bytes with 4-byte characters (~76 tokens, ~1,520 for 20 hits).
//!
//! Groups are compared on the full key; only the displayed label is made
//! single-line and shortened, so two sessions whose ids differ only past the
//! cap still get separate headers.
use super::search_format::{cap_chars, render_line, render_lines, Predecessor};
use crate::handlers::types::SearchResult;
use crate::AppState;
use chrono::NaiveDate;
use loomem_core::config::SearchGroupBy;
use loomem_core::storage::{Chunk, RocksDbStore};
use serde_json::Value;
use std::collections::HashMap;

#[cfg(test)]
mod tests;

/// Label for a day key whose date tag is empty or not a date.
const UNKNOWN_DATE: &str = "unknown date";

/// Character cap for a displayed header label (`…` appended when cut).
/// `source_session` has no length limit at ingest.
const MAX_HEADER_CHARS: usize = 80;

/// Kind prefixes `ingest_date::date_prefix` may put before the date.
const DATE_TAG_KINDS: [&str; 3] = ["event:", "observed:", "recorded:"];

/// The grouping actually applied: none for aggregation queries, whose
/// enumeration text must stay a flat numbered list.
pub(super) fn effective_mode(configured: SearchGroupBy, is_aggregation: bool) -> SearchGroupBy {
    if is_aggregation {
        SearchGroupBy::None
    } else {
        configured
    }
}

/// `render_grouped` with the configured `[mcp] search_group_by`, forced off
/// for aggregation queries — the `tool_search` call site.
pub(super) fn render_for(
    state: &AppState,
    hits: &[SearchResult],
    preds: &HashMap<String, Predecessor>,
    is_aggregation: bool,
) -> String {
    let mode = effective_mode(state.config.mcp.search_group_by, is_aggregation);
    render_grouped(&state.store, hits, preds, mode)
}

/// Render every hit like `render_lines`, with a `## <key>` header before each
/// hit that starts a new group. `SearchGroupBy::None` returns `render_lines`
/// unchanged. Headers sit outside `max_context_tokens` (see module docs).
pub(super) fn render_grouped(
    store: &RocksDbStore,
    hits: &[SearchResult],
    preds: &HashMap<String, Predecessor>,
    mode: SearchGroupBy,
) -> String {
    if mode == SearchGroupBy::None {
        return render_lines(store, hits, preds);
    }
    let mut text = String::new();
    let mut previous: Option<String> = None;
    for (i, r) in hits.iter().enumerate() {
        let key = hit_key(store, r, mode);
        if let Some(k) = key.as_deref().filter(|k| previous.as_deref() != Some(*k)) {
            text.push_str(&format!("## {}\n", header_label(k)));
        }
        previous = key;
        text.push_str(&render_line(store, i, r, preds.get(&r.id)));
    }
    text
}

/// Group key of one hit, from its chunk and the same date tag its result line
/// shows (`ingest_date::date_prefix` over chunk and result timestamp).
fn hit_key(store: &RocksDbStore, r: &SearchResult, mode: SearchGroupBy) -> Option<String> {
    let chunk = store.get_chunk(&r.id).ok().flatten();
    let timestamp = r
        .metadata
        .as_ref()
        .and_then(|meta| meta.get("timestamp"))
        .and_then(Value::as_i64);
    let ts = crate::handlers::ingest_date::date_prefix(chunk.as_ref(), timestamp);
    group_key(mode, chunk.as_ref(), &ts)
}

/// Group key of a hit (compared in full; displayed via `header_label`), or
/// `None` when grouping is off.
/// - `Session`: `session: <fragment source id>`, else `session: <source
///   session>`, else the day key (a chunk with no session groups by day).
/// - `Day`: the `YYYY-MM-DD` of the date tag `ts`, or `unknown date`.
pub(super) fn group_key(mode: SearchGroupBy, chunk: Option<&Chunk>, ts: &str) -> Option<String> {
    match mode {
        SearchGroupBy::None => None,
        SearchGroupBy::Session => Some(
            chunk
                .and_then(session_id)
                .map(|id| format!("session: {id}"))
                .unwrap_or_else(|| day_key(ts)),
        ),
        SearchGroupBy::Day => Some(day_key(ts)),
    }
}

/// The session a chunk came from: its canonical source when it is a source
/// fragment, else the caller-supplied `source.session`. Blank ids are ignored.
fn session_id(chunk: &Chunk) -> Option<&str> {
    let fragment = chunk.fragment.as_ref().map(|f| f.source_id.as_str());
    let session = chunk.source.as_ref().and_then(|s| s.session.as_deref());
    fragment
        .filter(|id| !id.trim().is_empty())
        .or_else(|| session.filter(|id| !id.trim().is_empty()))
}

/// A group key as displayed: every control character (line breaks included)
/// and U+2028/U+2029 replaced by a space, so a caller-supplied
/// `source_session` cannot open a header line of its own, then cut to
/// `MAX_HEADER_CHARS` characters so it cannot blow up the response.
pub(super) fn header_label(key: &str) -> String {
    let single_line: String = key
        .chars()
        .map(|c| match c {
            '\u{2028}' | '\u{2029}' => ' ',
            c if c.is_control() => ' ',
            c => c,
        })
        .collect();
    cap_chars(&single_line, MAX_HEADER_CHARS)
}

/// `YYYY-MM-DD` from a date tag (`2024-01-01`, `event:2024-01-01`,
/// `observed:…`, `recorded:…`, or an event date with a time part), else
/// `unknown date`.
fn day_key(ts: &str) -> String {
    let value = DATE_TAG_KINDS
        .iter()
        .find_map(|kind| ts.strip_prefix(kind))
        .unwrap_or(ts);
    value
        .get(..10)
        .filter(|day| NaiveDate::parse_from_str(day, "%Y-%m-%d").is_ok())
        .map_or_else(|| UNKNOWN_DATE.to_string(), str::to_string)
}
