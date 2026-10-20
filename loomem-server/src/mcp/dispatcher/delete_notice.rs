//! Explain record-level deletion when a retained transcript also exists.
use loomem_core::storage::{is_ingest_fact_shape, Chunk};

pub(super) fn retained_source(chunk: &Chunk) -> String {
    if !is_ingest_fact_shape(chunk) {
        return String::new();
    }
    let Some(source) = chunk
        .extraction_meta
        .as_ref()
        .and_then(|meta| meta.extracted_from.as_ref())
    else {
        return String::new();
    };
    format!(
        " Deleting this fact alone does not delete its source transcript {source} or other facts. To remove the conversation and its linked memories from retrieval, delete source {source}."
    )
}
