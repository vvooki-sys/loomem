//! Opt-in fragment persistence for `memory_ingest`: additive to the source,
//! never a gate for fact extraction.
//!
//! Byte-range contract: every stored fragment's `[byte_start, byte_end)` is
//! an exact slice of the source content *as persisted*. Two things make that
//! hold. `prepare` sanitises the content to a fixpoint, so the sink's own
//! re-application of the pipeline (`persist_chunk`) leaves the source alone;
//! this module then reads the source back from the sink and segments those
//! bytes — if they differ from what was handed in, no fragment is written
//! and the tool text says so. Each fragment is checked the same way before
//! its write (`align_to_sink`): when the pipeline would merely trim a slice
//! (the HTML pass runs on any slice holding a `<` or an entity and trims its
//! ends; injection patterns never change content), the range is narrowed to
//! the bytes that will actually be stored; a slice the pipeline
//! would rewrite is skipped and counted, never stored with a range that no
//! longer indexes into the source.
use super::{base_chunk, IngestContext, IngestPersistence, ToolResult};
use crate::handlers::ingest::sanitize::stored_content;
use loomem_core::fragments::{
    fragment_chunk, segment_source, CanonicalSource, SourceFragment, TokenCounter, TokenCounterKind,
};
use loomem_core::storage::Chunk;

/// What happened to the fragments of one source, rendered into the tool
/// result so a partial write is never silent.
pub(super) struct FragmentReport {
    pub stored: usize,
    pub failed: usize,
    /// Fragments not written because the sink would have rewritten their
    /// text (no exact byte range could describe the stored bytes).
    pub skipped: usize,
    /// Why nothing was segmented (`"segmentation failed: …"` or
    /// `"segmentation skipped: …"`).
    pub segmentation_error: Option<String>,
}

impl FragmentReport {
    fn empty() -> Self {
        Self {
            stored: 0,
            failed: 0,
            skipped: 0,
            segmentation_error: None,
        }
    }

    fn not_segmented(reason: String) -> Self {
        Self {
            segmentation_error: Some(reason),
            ..Self::empty()
        }
    }

    pub fn sentence(&self) -> String {
        if let Some(reason) = &self.segmentation_error {
            return format!("Stored 0 fragments ({reason}).");
        }
        let mut sentence = format!("Stored {} fragments", self.stored);
        if self.failed > 0 {
            sentence.push_str(&format!(", {} fragment writes failed", self.failed));
        }
        if self.skipped > 0 {
            sentence.push_str(&format!(
                ", {} fragments skipped (sanitizer would rewrite the slice)",
                self.skipped
            ));
        }
        sentence.push('.');
        sentence
    }
}

/// Persist the fragments of an already-persisted `source` through the same
/// sink. `None` when `[fragments]` is disabled (the default): the ingest flow
/// then performs exactly the writes it did before this module existed.
/// Whitespace-only fragments are skipped; a failed write is counted and the
/// remaining fragments are still attempted.
pub(super) async fn store(
    context: &IngestContext<'_>,
    source: &Chunk,
    sink: &impl IngestPersistence,
) -> Option<FragmentReport> {
    if !context.state.config.fragments.enabled {
        return None;
    }
    let persisted = match persisted_source(context, source, sink).await {
        Ok(persisted) => persisted,
        Err(reason) => {
            tracing::warn!(source_id = %source.id, %reason, "fragments not segmented");
            return Some(FragmentReport::not_segmented(reason));
        }
    };
    let fragments = match segment(context, &persisted) {
        Ok(fragments) => fragments,
        Err(reason) => {
            tracing::warn!(source_id = %source.id, %reason, "fragment segmentation failed");
            return Some(FragmentReport::not_segmented(format!(
                "segmentation failed: {reason}"
            )));
        }
    };
    Some(write_fragments(context, &persisted, fragments, sink).await)
}

/// The source as the sink stored it; an error when it cannot be read back or
/// differs from the content that was handed in.
async fn persisted_source(
    context: &IngestContext<'_>,
    source: &Chunk,
    sink: &impl IngestPersistence,
) -> Result<Chunk, String> {
    let stored = sink
        .persisted(context.state, &source.id)
        .await
        .map_err(|error| format!("segmentation skipped: source read-back failed: {error}"))?
        .ok_or_else(|| "segmentation skipped: persisted source not found".to_string())?;
    if stored.content != source.content {
        return Err("segmentation skipped: persisted source differs".to_string());
    }
    Ok(stored)
}

async fn write_fragments(
    context: &IngestContext<'_>,
    source: &Chunk,
    fragments: Vec<SourceFragment>,
    sink: &impl IngestPersistence,
) -> FragmentReport {
    let mut report = FragmentReport::empty();
    // Fragments inherit the source's trust and take the configured role; the
    // marker kind "fragment" keeps them apart from facts in lineage checks.
    // Like the source they are never consolidation input (`consolidated`),
    // so raw turns do not feed dream/L1 summaries or the pending-L0 counter.
    let mut template = base_chunk(context, String::new(), String::new(), "fragment");
    template.consolidated = true;
    template.provenance_role = context.state.config.fragments.provenance_role;
    template.trust_level = source.trust_level.clone();
    for fragment in fragments.iter().filter(|f| !f.text.trim().is_empty()) {
        let Some(aligned) = align_to_sink(&context.state.pii_filter, fragment) else {
            tracing::warn!(id = %fragment.id, "fragment skipped: sanitizer would rewrite the slice");
            report.skipped += 1;
            continue;
        };
        match sink
            .persist(context.state, fragment_chunk(&aligned, &template))
            .await
        {
            Ok(_) => report.stored += 1,
            Err(error) => {
                tracing::warn!(id = %fragment.id, error = ?error, "fragment write failed");
                report.failed += 1;
            }
        }
    }
    report
}

/// The fragment whose `text` and byte range are exactly what the sink will
/// store for it. Identity for most slices; a slice the pipeline only trims
/// is narrowed to the surviving bytes (still an exact slice of the source);
/// `None` when the stored text would not be a substring of the slice.
pub(super) fn align_to_sink(
    pii: &loomem_core::pii_filter::PiiFilter,
    fragment: &SourceFragment,
) -> Option<SourceFragment> {
    let stored = stored_content(pii, &fragment.text);
    if stored == fragment.text {
        return Some(fragment.clone());
    }
    if stored.is_empty() {
        return None;
    }
    let offset = fragment.text.find(&stored)?;
    let byte_start = fragment.byte_start + offset;
    Some(SourceFragment {
        byte_start,
        byte_end: byte_start + stored.len(),
        text: stored,
        ..fragment.clone()
    })
}

fn segment(context: &IngestContext<'_>, source: &Chunk) -> Result<Vec<SourceFragment>, String> {
    let cfg = &context.state.config.fragments;
    let counter = token_counter(context)?;
    let canonical = CanonicalSource {
        id: source.id.clone(),
        text: source.content.clone(),
        conversation_date: context.conversation_date.clone(),
    };
    segment_source(&canonical, cfg, counter.as_ref()).map_err(|error| error.to_string())
}

/// The configured counter. `"embedder"` is the loaded local model's own
/// tokenizer and is only available when that model is loaded — there is no
/// silent fallback to an approximation, because the budget would then no
/// longer be the embedder's.
fn token_counter<'a>(context: &'a IngestContext<'_>) -> Result<Box<dyn TokenCounter + 'a>, String> {
    let cfg = &context.state.config.fragments;
    if cfg.token_counter != TokenCounterKind::Embedder {
        return cfg
            .token_counter
            .counter()
            .map_err(|error| error.to_string());
    }
    let embedder = context.state.local_embedder.as_deref().ok_or_else(|| {
        "embedder tokenizer unavailable (no local embedding model loaded)".to_string()
    })?;
    let counter = embedder
        .token_counter()
        .map_err(|error| format!("embedder tokenizer unavailable: {error}"))?;
    Ok(Box::new(counter))
}

/// Append the fragment sentence to the tool text. Existing sentences stay
/// byte-identical; nothing is appended when fragments were not attempted.
pub(super) fn append_report(mut result: ToolResult, report: Option<&FragmentReport>) -> ToolResult {
    if let (Some(report), Some(block)) = (report, result.content.first_mut()) {
        block.text.push(' ');
        block.text.push_str(&report.sentence());
    }
    result
}
