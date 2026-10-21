//! The content pipeline `persist_chunk` applies before a write, as one pure
//! function, so a caller that derives byte ranges from text it is about to
//! store (fragments of a source) can predict the stored bytes exactly instead
//! of assuming the pipeline is a no-op.
use loomem_core::pii_filter::PiiFilter;

/// Outcome of the pre-storage pipeline for one piece of content.
pub(crate) struct PersistContent {
    /// Fully sanitised text (HTML/injection strip, then PII): what the index,
    /// entity extraction and the embedding queue receive.
    pub text: String,
    /// Replacement for `Chunk::content`, present only when a pass changed
    /// something — `None` leaves the chunk's own content untouched. This
    /// preserves the historical rule: the chunk is rewritten only on an HTML
    /// strip, an injection detection or a PII redaction.
    pub chunk_content: Option<String>,
    /// Number of PII redactions applied (logged by the caller).
    pub pii_redactions: usize,
}

/// Apply the pipeline to `content`.
pub(crate) fn persist_content(pii: &PiiFilter, content: &str) -> PersistContent {
    let sanitize_result = loomem_core::sanitizer::sanitize(content);
    let (text, pii_redactions) = pii.sanitize(&sanitize_result.content);
    let chunk_content = if !pii_redactions.is_empty() {
        Some(text.clone())
    } else if sanitize_result.html_stripped || sanitize_result.injection_detected {
        Some(sanitize_result.content)
    } else {
        None
    };
    PersistContent {
        text,
        chunk_content,
        pii_redactions: pii_redactions.len(),
    }
}

/// What `Chunk::content` holds after `persist_chunk` stores `content`.
pub(crate) fn stored_content(pii: &PiiFilter, content: &str) -> String {
    persist_content(pii, content)
        .chunk_content
        .unwrap_or_else(|| content.to_string())
}
