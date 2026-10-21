//! Opt-in input chunking and truncation-split retry for knowledge extraction.
//!
//! Both controls default off and are wired from
//! [`KnowledgeExtractionConfig`]: `input_chunk_tokens` splits a transcript at
//! line boundaries before the first call, `split_on_truncation` re-extracts a
//! chunk whose completion hit the output token limit as two line-aligned
//! halves. A truncated reply is never admitted (see `reply.rs`); the retry
//! only changes how much input the next call sees.

use tracing::debug;

use super::reply::parse_extraction_reply;
use super::{
    build_extraction_request, truncate_reason, ChunkFailure, ExtractedFact, ExtractionChat,
    KnowledgeExtractionConfig,
};

/// Maximum number of times one transcript chunk may be halved along a single
/// path (halves, then quarters).
pub(super) const MAX_SPLIT_DEPTH: usize = 2;

/// Maximum extra extraction calls one transcript chunk may spend on split
/// retries: two halves, then at most one of them split again. Together with
/// [`MAX_SPLIT_DEPTH`] this bounds a chunk at 1 + 4 calls.
pub(super) const MAX_SPLIT_CALLS_PER_CHUNK: usize = 4;

/// Tally of one transcript chunk's extraction calls (original plus any split
/// retries), merged into `ExtractionOutcome` by the caller.
#[derive(Debug, Default)]
pub(super) struct ChunkRun {
    /// Raw facts from every completed call, in transcript order, before the
    /// caller's confidence filter.
    pub facts: Vec<ExtractedFact>,
    /// Final failures: non-2xx / transport / parse, and truncations that were
    /// not (or could not be) split further.
    pub failures: Vec<ChunkFailure>,
    pub calls_attempted: usize,
    pub calls_completed: usize,
    pub calls_truncated: usize,
}

/// Extract one transcript chunk, re-extracting line-aligned halves when the
/// reply is truncated and `split_on_truncation` allows it.
///
/// Sub-chunks keep the parent's `chunk_index` (boundary marker id and
/// `ChunkFailure::chunk_index`). An explicit stack replaces async recursion;
/// the right half is pushed first so the earlier transcript text is always
/// extracted first, which keeps fact order deterministic.
pub(super) async fn extract_chunk_with_split(
    chat: &impl ExtractionChat,
    config: &KnowledgeExtractionConfig,
    system_prompt: &str,
    chunk_index: usize,
    chunk_text: &str,
) -> ChunkRun {
    let mut run = ChunkRun::default();
    let mut budget = MAX_SPLIT_CALLS_PER_CHUNK;
    let mut pending = vec![(chunk_text.to_string(), 0usize)];

    while let Some((text, depth)) = pending.pop() {
        let request_body = build_extraction_request(
            &config.model,
            system_prompt,
            chunk_index,
            &text,
            config.max_tokens,
        );
        run.calls_attempted += 1;
        match extract_chunk(chat, &request_body, chunk_index).await {
            Ok(facts) => {
                run.calls_completed += 1;
                run.facts.extend(facts);
            }
            Err(failure) if failure.is_truncated() => {
                run.calls_truncated += 1;
                match split_for_retry(config, &text, depth, budget) {
                    Some((left, right)) => {
                        budget -= 2;
                        debug!(
                            "Extraction chunk {chunk_index} truncated at depth {depth}; retrying as two halves ({} + {} chars)",
                            left.len(),
                            right.len()
                        );
                        pending.push((right, depth + 1));
                        pending.push((left, depth + 1));
                    }
                    None => run.failures.push(failure),
                }
            }
            Err(failure) => run.failures.push(failure),
        }
    }
    run
}

/// Halves to retry after a truncated reply, or `None` when the retry is
/// disabled, the depth / call budget is exhausted, or the text has no line
/// boundary that leaves two non-blank halves.
fn split_for_retry(
    config: &KnowledgeExtractionConfig,
    text: &str,
    depth: usize,
    budget: usize,
) -> Option<(String, String)> {
    if !config.split_on_truncation || depth >= MAX_SPLIT_DEPTH || budget < 2 {
        return None;
    }
    split_at_line_boundary(text)
}

/// Run one chunk's request → parse pipeline. `Err` carries the per-chunk
/// failure (HTTP non-2xx, transport, or parse) for aggregation (/157 S1).
async fn extract_chunk(
    chat: &impl ExtractionChat,
    request_body: &serde_json::Value,
    chunk_index: usize,
) -> Result<Vec<ExtractedFact>, ChunkFailure> {
    let reply = match chat.chat(request_body).await {
        Ok(r) => r,
        Err(e) => {
            return Err(ChunkFailure {
                chunk_index,
                status: None,
                reason: truncate_reason(&format!("{e:#}")),
            })
        }
    };

    if !(200..300).contains(&reply.status) {
        return Err(ChunkFailure {
            chunk_index,
            status: Some(reply.status),
            reason: truncate_reason(&reply.body),
        });
    }

    let facts = parse_extraction_reply(&reply, chunk_index)?;
    // Diagnostic for the silent-degradation signature (server-degradation brief
    // §4.1): a 2xx that parses to zero facts. `body_len` separates a healthy
    // "model legitimately found nothing" (full body) from a degraded transport
    // returning a near-empty body. Pairs with the windowed extraction_empty
    // counter; kept at debug so normal empty ingests don't spam logs.
    if facts.is_empty() {
        debug!(
            "Extraction chunk {chunk_index} returned 2xx with zero facts (status={}, body_len={})",
            reply.status,
            reply.body.len()
        );
    }
    Ok(facts)
}

/// Greedy line-aligned chunking: lines (newline included) are appended to the
/// current chunk while it stays within `max_chars` characters; a line that
/// does not fit starts the next chunk. A single line longer than `max_chars`
/// becomes a chunk of its own — lines are never cut. Concatenating the chunks
/// reproduces `text`; empty input yields one empty chunk, like the legacy
/// path.
pub(super) fn line_aligned_chunks(text: &str, max_chars: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut current_chars = 0usize;
    for line in text.split_inclusive('\n') {
        let line_chars = line.chars().count();
        if current_chars > 0 && current_chars + line_chars > max_chars {
            chunks.push(std::mem::take(&mut current));
            current_chars = 0;
        }
        current.push_str(line);
        current_chars += line_chars;
    }
    if !current.is_empty() || chunks.is_empty() {
        chunks.push(current);
    }
    chunks
}

/// Split `text` at the newline closest to its character midpoint such that
/// both halves are non-blank (the boundary newline itself is dropped). `None`
/// when no such newline exists, e.g. a single line.
fn split_at_line_boundary(text: &str) -> Option<(String, String)> {
    let mid = text.chars().count() / 2;
    let mut best: Option<(usize, usize)> = None; // (distance to mid, byte index)
    for (char_idx, (byte_idx, ch)) in text.char_indices().enumerate() {
        if ch != '\n' {
            continue;
        }
        let (left, right) = (&text[..byte_idx], &text[byte_idx + 1..]);
        if left.trim().is_empty() || right.trim().is_empty() {
            continue;
        }
        let distance = char_idx.abs_diff(mid);
        if best.is_none_or(|(d, _)| distance < d) {
            best = Some((distance, byte_idx));
        }
    }
    best.map(|(_, byte_idx)| {
        (
            text[..byte_idx].to_string(),
            text[byte_idx + 1..].to_string(),
        )
    })
}

#[cfg(test)]
mod tests;
