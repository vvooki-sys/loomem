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
    /// something — `None` leaves the chunk's own content untouched. Compare
    /// actual bytes too: entity decoding and whitespace normalization can
    /// change text without setting the HTML-tag or injection flags.
    pub chunk_content: Option<String>,
    /// Number of PII redactions applied (logged by the caller).
    pub pii_redactions: usize,
}

/// Apply the pipeline to `content`.
pub(crate) fn persist_content(pii: &PiiFilter, content: &str) -> PersistContent {
    let sanitize_result = loomem_core::sanitizer::sanitize(content);
    let (text, pii_redactions) = pii.sanitize(&sanitize_result.content);
    let chunk_content = if text != content || !pii_redactions.is_empty() {
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

#[cfg(test)]
mod tests {
    use super::*;
    use loomem_core::pii_filter::PiiConfig;

    #[test]
    fn canonical_text_matches_embedding_input_after_unflagged_changes() {
        let pii = PiiFilter::new(PiiConfig::default()).expect("PII filter");
        for (input, expected) in [
            ("Tom &amp; Jerry", "Tom & Jerry"),
            ("  value < 3  ", "value < 3"),
            ("value < 3    units", "value < 3  units"),
        ] {
            let prepared = persist_content(&pii, input);
            assert_eq!(prepared.text, expected);
            assert_eq!(prepared.chunk_content.as_deref(), Some(expected));
            assert_eq!(stored_content(&pii, input), prepared.text);
        }
    }

    #[test]
    fn unflagged_normalization_preserves_embedding_publication() {
        use loomem_core::storage::{Chunk, RocksDbConfig, RocksDbStore};
        let dir = tempfile::tempdir().expect("temp DB");
        let store = RocksDbStore::open(
            dir.path(),
            &RocksDbConfig {
                max_open_files: 100,
                compression: "lz4".into(),
                write_buffer_size: 4 * 1024 * 1024,
                max_write_buffer_number: 2,
            },
        )
        .expect("open DB");
        let pii = PiiFilter::new(PiiConfig::default()).expect("PII filter");
        let input = "  synthetic value < 3  ";
        let prepared = persist_content(&pii, input);
        let mut chunk: Chunk = serde_json::from_value(serde_json::json!({
            "id": "synthetic-fragment", "content": input, "stream": "test", "level": 0,
            "score": 1.0, "timestamp": 1, "consolidated": false,
            "dormant": false, "in_progress": false
        }))
        .expect("chunk");
        // Reproduce the old publication failure with unchanged canonical bytes.
        store.store_chunk(&chunk).expect("store old bytes");
        assert!(store
            .embedding_target(&chunk.id, &prepared.text)
            .unwrap()
            .is_none());
        chunk.content = prepared.chunk_content.unwrap_or_else(|| input.into());
        store.store_chunk(&chunk).expect("store canonical bytes");
        let target = store
            .embedding_target(&chunk.id, &prepared.text)
            .unwrap()
            .expect("canonical text must admit embedding work");
        assert!(store
            .store_embedding_if_current(&target, vec![1.0, 0.0])
            .unwrap());
        assert_eq!(
            store.get_embedding(&chunk.id).unwrap(),
            Some(vec![1.0, 0.0])
        );
    }

    #[test]
    fn unchanged_text_needs_no_canonical_replacement() {
        let pii = PiiFilter::new(PiiConfig::default()).expect("PII filter");
        // Plain text skips `strip_html` entirely; "a < b" runs through it and
        // must still come out byte-identical with no replacement.
        for input in ["A synthetic fact.", "a < b"] {
            let prepared = persist_content(&pii, input);
            assert_eq!(prepared.text, input);
            assert!(prepared.chunk_content.is_none());
            assert_eq!(stored_content(&pii, input), input);
        }
    }
}
