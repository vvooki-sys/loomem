//! Date provenance for the versioned MCP ingestion contract only.
use chrono::{DateTime, NaiveDate};
use loomem_core::storage::{is_ingest_fact_shape, is_ingest_source, Chunk};

/// Date prefix contents (without brackets), or `None` for unchanged legacy rendering.
/// Only recognized server-owned source/fact provenance enables these labels.
pub(crate) fn new_ingest_date_prefix(chunk: &Chunk) -> Option<String> {
    if !is_ingest_source(chunk) && !is_ingest_fact_shape(chunk) {
        return None;
    }
    let marker = chunk.metadata.as_ref()?.get("_loomem_ingest")?;
    let event_date = chunk
        .extraction_meta
        .as_ref()
        .and_then(|meta| meta.event_date.as_deref())
        .and_then(canonical_date);
    if let Some(date) = event_date {
        return Some(format!("event:{date}"));
    }
    let observed = marker
        .get("conversation_date")
        .and_then(|date| date.as_str())
        .and_then(canonical_date);
    if let Some(date) = observed {
        return Some(format!("observed:{date}"));
    }
    let recorded = i64::try_from(chunk.timestamp)
        .ok()
        .and_then(|timestamp| DateTime::from_timestamp(timestamp, 0))
        .map(|date| date.format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| "unknown".to_string());
    Some(format!("recorded:{recorded}"))
}

/// Shared new/legacy date selection; callers retain their existing fallback timestamp.
pub(crate) fn date_prefix(chunk: Option<&Chunk>, timestamp: Option<i64>) -> String {
    chunk
        .and_then(new_ingest_date_prefix)
        .or_else(|| chunk.and_then(|chunk| chunk.extraction_meta.as_ref()?.event_date.clone()))
        .unwrap_or_else(|| {
            timestamp
                .and_then(|value| DateTime::from_timestamp(value, 0))
                .map(|date| date.format("%Y-%m-%d").to_string())
                .unwrap_or_default()
        })
}

/// Render only authoritative live same-stream memories; stale index text is not evidence.
pub(crate) fn live_context_line(
    store: &loomem_core::storage::RocksDbStore,
    stream: &str,
    hit: &loomem_core::tantivy_index::SearchResult,
) -> Option<String> {
    let chunk = store.get_chunk(&hit.id).ok()??;
    if chunk.stream != stream
        || chunk.deleted_at.is_some()
        || chunk.dormant
        || !chunk.is_latest
        || chunk.superseded_by.is_some()
        || chunk.in_progress
    {
        return None;
    }
    Some(context_line(&chunk, hit.timestamp))
}

/// Format one context bullet, preserving the legacy empty-date form.
pub(crate) fn context_line(chunk: &Chunk, timestamp: i64) -> String {
    let prefix = date_prefix(Some(chunk), Some(timestamp));
    if prefix.is_empty() {
        format!("- {}\n", chunk.content)
    } else {
        format!("- [{prefix}] {}\n", chunk.content)
    }
}

fn canonical_date(value: &str) -> Option<&str> {
    let parsed = NaiveDate::parse_from_str(value, "%Y-%m-%d").ok()?;
    (parsed.format("%Y-%m-%d").to_string() == value).then_some(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    fn chunk(marker: Value, event_date: Option<&str>) -> Chunk {
        let mut chunk: Chunk = serde_json::from_value(json!({
            "id":"date-fixture", "content":"unchanged source", "stream":"test", "level":1,
            "score":1.0, "timestamp":1704067200, "consolidated":false,
            "dormant":false, "in_progress":false, "metadata":marker
        }))
        .unwrap();
        chunk.trust_level = Some("a2".into());
        chunk.persistent = true;
        let kind = chunk
            .metadata
            .as_ref()
            .and_then(|value| value.get("_loomem_ingest"))
            .and_then(|value| value.get("kind"))
            .and_then(Value::as_str);
        if kind == Some("source") {
            chunk.level = 0;
            chunk.consolidated = true;
            chunk.provenance_role = loomem_core::storage::ProvenanceRole::Evidence;
            chunk.source = Some(loomem_core::source_tag::SourceTag::from_agent(
                "mcp-ingest-source",
            ));
        } else {
            let source_id = "00000000-0000-4000-8000-000000000001".to_string();
            chunk.source_ids = Some(vec![source_id.clone()]);
            chunk.source = Some(loomem_core::source_tag::SourceTag::from_agent("mcp-ingest"));
            let fact = loomem_core::memory_extractor::ExtractedFact {
                content: chunk.content.clone(),
                fact_type: "event".into(),
                subject: None,
                event_date: event_date.map(str::to_owned),
                event_date_context: None,
                confidence: 0.9,
                attributed_to: None,
            };
            chunk.extraction_meta = Some(fact.to_extraction_meta(Some(source_id), "test"));
        }
        chunk
    }

    fn marker(kind: &str, date: Value) -> Value {
        json!({"_loomem_ingest":{"version":1,"kind":kind,"conversation_date":date}})
    }

    #[test]
    fn known_event_wins_without_rewriting_source_or_metadata() {
        let chunk = chunk(marker("fact", json!("2026-10-20")), Some("2020-02-03"));
        let before = serde_json::to_value(&chunk).unwrap();
        assert_eq!(
            new_ingest_date_prefix(&chunk).as_deref(),
            Some("event:2020-02-03")
        );
        assert_eq!(serde_json::to_value(&chunk).unwrap(), before);
    }

    #[test]
    fn unknown_event_uses_validated_observation_then_recorded_date() {
        for kind in ["source", "fact"] {
            let source = chunk(marker(kind, json!("2026-10-20")), None);
            assert_eq!(
                new_ingest_date_prefix(&source).as_deref(),
                Some("observed:2026-10-20")
            );
            for date in [
                Value::Null,
                json!("invalid"),
                json!("2026-1-2"),
                json!("2026-02-30"),
            ] {
                let source = chunk(marker(kind, date), None);
                assert_eq!(
                    new_ingest_date_prefix(&source).as_deref(),
                    Some("recorded:2024-01-01")
                );
            }
        }
        let mut source = chunk(marker("source", Value::Null), None);
        source.timestamp = u64::MAX;
        assert_eq!(
            new_ingest_date_prefix(&source).as_deref(),
            Some("recorded:unknown")
        );
    }

    #[test]
    fn forged_markers_without_owned_provenance_do_not_opt_in() {
        let mut source = chunk(marker("source", json!("2026-10-20")), None);
        source.provenance_role = loomem_core::storage::ProvenanceRole::Claim;
        assert_eq!(new_ingest_date_prefix(&source), None);
        let valid = chunk(marker("fact", json!("2026-10-20")), Some("2020-02-03"));
        let mut no_ids = valid.clone();
        no_ids.source_ids = None;
        let mut empty_ids = valid.clone();
        empty_ids.source_ids = Some(vec![]);
        let mut no_extraction = valid.clone();
        no_extraction.extraction_meta = None;
        let mut missing_parent = valid.clone();
        missing_parent
            .extraction_meta
            .as_mut()
            .unwrap()
            .extracted_from = None;
        let mut mismatch = valid.clone();
        mismatch.extraction_meta.as_mut().unwrap().extracted_from =
            Some("00000000-0000-4000-8000-000000000002".into());
        let mut invalid_uuid = valid.clone();
        invalid_uuid.source_ids = Some(vec!["invalid".into()]);
        invalid_uuid
            .extraction_meta
            .as_mut()
            .unwrap()
            .extracted_from = Some("invalid".into());
        let mut wrong_trust = valid;
        wrong_trust.trust_level = Some("a1".into());
        for forged in [
            no_ids,
            empty_ids,
            no_extraction,
            missing_parent,
            mismatch,
            invalid_uuid,
            wrong_trust,
        ] {
            assert_eq!(new_ingest_date_prefix(&forged), None);
        }
    }

    #[test]
    fn absent_unknown_or_malformed_markers_keep_legacy_rendering() {
        for metadata in [
            Value::Null,
            json!({}),
            json!({"conversation_date":"2026-10-20"}),
            json!({"_loomem_ingest":{"version":2,"kind":"fact"}}),
            json!({"_loomem_ingest":{"version":1,"kind":"unrecognized"}}),
            json!({"_loomem_ingest":{"version":"1","kind":"fact"}}),
        ] {
            let source = chunk(metadata, Some("2020-02-03"));
            assert_eq!(new_ingest_date_prefix(&source), None);
        }
    }
}
