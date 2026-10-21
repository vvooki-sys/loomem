//! Sink policy for generated dream and consolidation records.
use crate::pii_filter::PiiFilter;
use crate::storage::Chunk;

/// Generated text never acquires more authority than its weakest source,
/// and is capped at assistant-derived trust even when all sources are A1.
pub(crate) fn derived_trust<'a>(sources: impl Iterator<Item = &'a Chunk>) -> &'static str {
    match sources
        .map(|c| crate::contradiction::trust_rank(c.trust_level.as_deref()))
        .min()
        .unwrap_or(0)
    {
        0 => "unknown",
        1 => "b",
        _ => "a2",
    }
}

/// Run the configured sanitizer and PII policy on generated durable text
/// before any persistence, indexing, entity extraction, or embedding sink.
/// Structural source identifiers remain unchanged to preserve provenance.
pub(crate) fn protect_derived_chunk(chunk: &mut Chunk, pii: &PiiFilter) {
    chunk.content = pii.redact_for_sink(&chunk.content);
    redact_optional(&mut chunk.memory_type, pii);
    if let Some(meta) = chunk.extraction_meta.as_mut() {
        for field in [
            &mut meta.subject,
            &mut meta.event_date,
            &mut meta.event_date_context,
            &mut meta.original_content,
            &mut meta.topic,
            &mut meta.attributed_to,
        ] {
            redact_optional(field, pii);
        }
    }
}

fn redact_optional(field: &mut Option<String>, pii: &PiiFilter) {
    if let Some(text) = field {
        *text = pii.redact_for_sink(text);
    }
}

/// A new UUID summary is embedded before publication for the quality gate.
/// After publication, bind its vector to the exact persisted content/generation.
pub(crate) fn store_generated_embedding(
    store: &crate::storage::RocksDbStore,
    chunk: &Chunk,
    embedding: Vec<f32>,
) -> anyhow::Result<()> {
    if let Some(target) = store.embedding_target(&chunk.id, &chunk.content)? {
        store.store_embedding_if_current(&target, embedding)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pii_filter::PiiConfig;

    fn chunk(trust: Option<&str>) -> Chunk {
        serde_json::from_value(serde_json::json!({
            "id":"L0:test", "content":"<b>synthetic@example.test</b>",
            "stream":"s", "level":0, "score":1.0, "timestamp":1000,
            "consolidated":false, "dormant":false, "in_progress":false,
            "trust_level":trust,
            "memory_type":"<b>synthetic@example.test</b>",
            "extraction_meta":{
                "fact_type":"fact", "confidence":0.9,
                "subject":"<b>synthetic@example.test</b>",
                "event_date":"<b>synthetic@example.test</b>",
                "event_date_context":"<b>synthetic@example.test</b>",
                "original_content":"<b>synthetic@example.test</b>",
                "topic":"<b>synthetic@example.test</b>",
                "attributed_to":"<b>synthetic@example.test</b>"
            }
        }))
        .expect("fixture")
    }

    #[test]
    fn authority_is_capped_and_preserves_weakest_source() {
        for (tiers, expected) in [
            (vec![None], "a2"),
            (vec![Some("a1"), Some("a2")], "a2"),
            (vec![Some("a1"), Some("b")], "b"),
            (vec![Some("b")], "b"),
            (vec![Some("invalid"), Some("a1")], "unknown"),
            (vec![], "unknown"),
        ] {
            let chunks: Vec<_> = tiers.into_iter().map(chunk).collect();
            assert_eq!(derived_trust(chunks.iter()), expected);
        }
    }

    #[test]
    fn every_generated_text_field_uses_configured_sink_policy() {
        let filter = PiiFilter::new(PiiConfig {
            blocklist_file: String::new(),
            ..Default::default()
        })
        .expect("filter");
        let mut derived = chunk(Some("b"));
        protect_derived_chunk(&mut derived, &filter);
        let json = serde_json::to_string(&derived).expect("serialize");
        assert!(!json.contains("synthetic@example.test"));
        assert!(!json.contains("<b>"));
        assert_eq!(
            derived.content,
            filter.redact_for_sink("synthetic@example.test")
        );
        assert_eq!(derived.id, "L0:test");
    }

    #[test]
    fn disabled_pii_policy_is_respected_but_html_is_still_removed() {
        let filter = PiiFilter::new(PiiConfig {
            enabled: false,
            blocklist_file: String::new(),
            ..Default::default()
        })
        .expect("filter");
        let mut derived = chunk(Some("a2"));
        protect_derived_chunk(&mut derived, &filter);
        assert_eq!(derived.content, "synthetic@example.test");
    }
}
