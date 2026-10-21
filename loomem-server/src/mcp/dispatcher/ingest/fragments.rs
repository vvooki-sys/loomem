//! Opt-in fragment persistence for `memory_ingest`: additive to the source,
//! never a gate for fact extraction.
//!
//! Sanitisation invariant: the fragment text is a byte slice of the source
//! content as handed to the sink. That content is already sanitised —
//! `prepare` applies `PiiFilter::redact_for_sink` (HTML/injection strip + PII
//! redaction), and `persist_chunk` re-applies the same idempotent pipeline on
//! every write, so a fragment going through the sink changes nothing. No
//! second pass is needed here and none is attempted; the tests pin the
//! persisted source content to the fragment slices.
use super::{base_chunk, IngestContext, IngestPersistence, ToolResult};
use loomem_core::fragments::{fragment_chunk, segment_source, CanonicalSource, SourceFragment};
use loomem_core::storage::Chunk;

/// What happened to the fragments of one source, rendered into the tool
/// result so a partial write is never silent.
pub(super) struct FragmentReport {
    pub stored: usize,
    pub failed: usize,
    pub segmentation_error: Option<String>,
}

impl FragmentReport {
    pub fn sentence(&self) -> String {
        if let Some(reason) = &self.segmentation_error {
            return format!("Stored 0 fragments (segmentation failed: {reason}).");
        }
        if self.failed > 0 {
            return format!(
                "Stored {} fragments, {} fragment writes failed.",
                self.stored, self.failed
            );
        }
        format!("Stored {} fragments.", self.stored)
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
    let cfg = &context.state.config.fragments;
    if !cfg.enabled {
        return None;
    }
    let mut report = FragmentReport {
        stored: 0,
        failed: 0,
        segmentation_error: None,
    };
    let fragments = match segment(context, source) {
        Ok(fragments) => fragments,
        Err(reason) => {
            tracing::warn!(source_id = %source.id, %reason, "fragment segmentation failed");
            report.segmentation_error = Some(reason);
            return Some(report);
        }
    };
    // Fragments inherit the source's trust and take the configured role; the
    // marker kind "fragment" keeps them apart from facts in lineage checks.
    let mut template = base_chunk(context, String::new(), String::new(), "fragment");
    template.provenance_role = cfg.provenance_role;
    template.trust_level = source.trust_level.clone();
    for fragment in fragments.iter().filter(|f| !f.text.trim().is_empty()) {
        match sink
            .persist(context.state, fragment_chunk(fragment, &template))
            .await
        {
            Ok(_) => report.stored += 1,
            Err(error) => {
                tracing::warn!(id = %fragment.id, error = ?error, "fragment write failed");
                report.failed += 1;
            }
        }
    }
    Some(report)
}

fn segment(context: &IngestContext<'_>, source: &Chunk) -> Result<Vec<SourceFragment>, String> {
    let cfg = &context.state.config.fragments;
    let counter = cfg
        .token_counter
        .counter()
        .map_err(|error| error.to_string())?;
    let canonical = CanonicalSource {
        id: source.id.clone(),
        text: source.content.clone(),
        conversation_date: context.conversation_date.clone(),
    };
    segment_source(&canonical, cfg, counter.as_ref()).map_err(|error| error.to_string())
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
