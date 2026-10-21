//! Evidence renderer — the one place that turns a stored [`Chunk`] into the
//! additive `evidence` object every read surface exposes (`/v1/search` hits,
//! `GET /api/memories/:id`, the `memory_search` text line).
//!
//! Why: conversation dates, authorship and source provenance exist in engine
//! metadata but were dropped at the product boundary, so a reader could not
//! tell when or by whom a retrieved statement was made. The object carries
//! exactly seven keys, all always present; an unknown value is JSON `null`,
//! never a substituted date (a storage or conversation date is never passed
//! off as an event date). Only calendar-valid ISO `YYYY-MM-DD` dates are
//! emitted; anything else becomes `null`.
//!
//! Pure: no I/O, no config. Surfaces gate it on `[search] evidence_fields`.

use chrono::NaiveDate;
use loomem_core::storage::{is_ingest_fact_shape, is_ingest_source, Chunk, ExtractionMeta};
use serde::Serialize;

/// Additive evidence object. Field set is frozen with the benchmark reader:
/// adding a key breaks its validation, so extend only by agreement.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Evidence {
    /// Normalised author role (`user`, `assistant`, `system`), or `null`.
    pub author: Option<String>,
    /// Date the conversation took place, as stored.
    pub conversation_date: Option<String>,
    /// Date the described event happened, as extracted.
    pub event_date: Option<String>,
    /// `resolved` when the engine recorded the temporal cue the date was
    /// resolved from, `unknown` otherwise; `null` exactly when `event_date`
    /// is `null`. See [`event_date_provenance`].
    pub event_date_provenance: Option<&'static str>,
    /// Canonical source chunk this memory was cut or extracted from.
    pub source_id: Option<String>,
    /// Byte range of a fragment within its source: both set or both `null`.
    pub byte_start: Option<usize>,
    pub byte_end: Option<usize>,
}

/// Author roles accepted from `extraction_meta.attributed_to`. The extractor
/// prompt asks for `user`/`assistant`; anything else (a person's name, free
/// text) is not a role and must not be surfaced as one.
const AUTHOR_ROLES: [&str; 3] = ["user", "assistant", "system"];

/// Render the evidence object for one chunk.
#[must_use]
pub fn render_evidence(chunk: &Chunk) -> Evidence {
    let meta = chunk.extraction_meta.as_ref();
    let fragment = chunk.fragment.as_ref();
    let event_date = meta
        .and_then(|meta| meta.event_date.as_deref())
        .and_then(|date| iso_date(date, "event_date"));
    let (byte_start, byte_end) = fragment
        .map(|fragment| (fragment.byte_start, fragment.byte_end))
        .filter(|(start, end)| end >= start)
        .unzip();
    Evidence {
        author: fragment
            .and_then(|fragment| fragment.role.clone())
            .or_else(|| attributed_role(meta))
            .filter(|role| !role.is_empty()),
        conversation_date: fragment
            .and_then(|fragment| fragment.conversation_date.as_deref())
            .or_else(|| ingest_conversation_date(chunk))
            .and_then(|date| iso_date(date, "conversation_date")),
        event_date_provenance: event_date.is_some().then(|| event_date_provenance(meta)),
        event_date,
        source_id: fragment
            .map(|fragment| fragment.source_id.clone())
            .or_else(|| chunk.source_ids.as_ref()?.first().cloned())
            .filter(|id| !id.is_empty()),
        byte_start,
        byte_end,
    }
}

/// Evidence for one search hit under the `[search] evidence_fields` flag:
/// `None` while the flag is off (key omitted), otherwise always an object —
/// all-null when the chunk could not be loaded — so every hit carries it.
#[must_use]
pub fn hit_evidence(enabled: bool, chunk: Option<&Chunk>) -> Option<Evidence> {
    enabled.then(|| chunk.map_or_else(Evidence::default, render_evidence))
}

/// Text tag for one `memory_search` line: empty when the hit carries no
/// evidence object (flag off), so the line stays byte-identical.
#[must_use]
pub fn hit_text_suffix(evidence: Option<&Evidence>) -> String {
    evidence.map(evidence_text_suffix).unwrap_or_default()
}

/// `attributed_to` when it names a role; names and free text yield `None`.
fn attributed_role(meta: Option<&ExtractionMeta>) -> Option<String> {
    let role = meta?.attributed_to.as_deref()?.trim().to_ascii_lowercase();
    AUTHOR_ROLES.contains(&role.as_str()).then_some(role)
}

/// The ingest-time conversation date recorded on server-owned source and
/// fact records (`metadata._loomem_ingest.conversation_date`). Gated on the
/// owned provenance shape, like the date labels, so a client-supplied
/// metadata key cannot forge a conversation date.
fn ingest_conversation_date(chunk: &Chunk) -> Option<&str> {
    if !is_ingest_source(chunk) && !is_ingest_fact_shape(chunk) {
        return None;
    }
    chunk
        .metadata
        .as_ref()?
        .get("_loomem_ingest")?
        .get("conversation_date")?
        .as_str()
}

/// How the event date was obtained. The engine records the relative cue a
/// date was resolved from (`event_date_context`: "yesterday", "two weeks
/// ago", …) but no flag saying a date was literal in the source, and some
/// writers (dream, the backfill probe) record no cue at all. So a recorded
/// cue is `resolved`; everything else is honestly `unknown`. `explicit` is
/// reserved until the extractor persists that distinction.
fn event_date_provenance(meta: Option<&ExtractionMeta>) -> &'static str {
    let has_cue = meta
        .and_then(|meta| meta.event_date_context.as_deref())
        .is_some_and(|cue| !cue.trim().is_empty());
    if has_cue {
        "resolved"
    } else {
        "unknown"
    }
}

/// `value` when it is a calendar-valid `YYYY-MM-DD` in canonical spelling.
fn iso_date(value: &str, field: &'static str) -> Option<String> {
    let canonical = NaiveDate::parse_from_str(value, "%Y-%m-%d")
        .ok()
        .is_some_and(|parsed| parsed.format("%Y-%m-%d").to_string() == value);
    if !canonical {
        tracing::debug!(field, "evidence: dropping non-ISO date");
    }
    canonical.then(|| value.to_string())
}

/// Compact bracketed suffix for the `memory_search` plain-text surface, e.g.
/// ` [author: user; conversation: 2023-03-19; event: 2023-03-01 (resolved);
/// source: <id>#0-120]`. Null parts are omitted; an all-null evidence
/// object renders as an empty string so the line is unchanged.
#[must_use]
pub fn evidence_text_suffix(evidence: &Evidence) -> String {
    let mut parts: Vec<String> = Vec::with_capacity(4);
    if let Some(author) = &evidence.author {
        parts.push(format!("author: {author}"));
    }
    if let Some(date) = &evidence.conversation_date {
        parts.push(format!("conversation: {date}"));
    }
    if let (Some(date), Some(provenance)) = (&evidence.event_date, evidence.event_date_provenance) {
        parts.push(format!("event: {date} ({provenance})"));
    }
    if let Some(source) = &evidence.source_id {
        let range = match (evidence.byte_start, evidence.byte_end) {
            (Some(start), Some(end)) => format!("#{start}-{end}"),
            _ => String::new(),
        };
        parts.push(format!("source: {source}{range}"));
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!(" [{}]", parts.join("; "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use loomem_core::fragments::FragmentMeta;
    use loomem_core::storage::{FactType, ProvenanceRole};
    use serde_json::json;

    fn chunk() -> Chunk {
        serde_json::from_value(json!({
            "id": "c1", "content": "text", "stream": "s", "level": 0, "score": 1.0,
            "timestamp": 1_700_000_000, "consolidated": false, "dormant": false,
            "in_progress": false
        }))
        .unwrap()
    }

    fn meta(
        event_date: Option<&str>,
        cue: Option<&str>,
        attributed_to: Option<&str>,
    ) -> ExtractionMeta {
        ExtractionMeta {
            fact_type: FactType::Event,
            subject: None,
            event_date: event_date.map(str::to_owned),
            event_date_context: cue.map(str::to_owned),
            supersedes: None,
            superseded_by: None,
            confidence: 0.9,
            extracted_from: None,
            extraction_model: None,
            original_content: None,
            topic: None,
            attributed_to: attributed_to.map(str::to_owned),
        }
    }

    fn fragment_chunk() -> Chunk {
        let mut chunk = chunk();
        chunk.source_ids = Some(vec!["src-1".into()]);
        chunk.fragment = Some(FragmentMeta {
            source_id: "src-1".into(),
            byte_start: 10,
            byte_end: 42,
            turn_index: 1,
            role: Some("assistant".into()),
            conversation_date: Some("2023-03-19".into()),
        });
        chunk
    }

    fn ingest_fact_chunk() -> Chunk {
        let mut chunk = chunk();
        let source = "00000000-0000-4000-8000-000000000001".to_string();
        chunk.metadata = Some(json!({"_loomem_ingest": {
            "version": 1, "kind": "fact", "conversation_date": "2023-03-19"}}));
        chunk.source_ids = Some(vec![source.clone()]);
        chunk.source = Some(loomem_core::source_tag::SourceTag::from_agent("mcp-ingest"));
        chunk.trust_level = Some("a2".into());
        chunk.provenance_role = ProvenanceRole::Claim;
        let mut meta = meta(Some("2023-03-01"), Some("two weeks ago"), Some("user"));
        meta.extracted_from = Some(source);
        chunk.extraction_meta = Some(meta);
        chunk
    }

    #[test]
    fn fragment_chunk_renders_role_date_and_byte_range() {
        let evidence = render_evidence(&fragment_chunk());
        assert_eq!(
            evidence,
            Evidence {
                author: Some("assistant".into()),
                conversation_date: Some("2023-03-19".into()),
                event_date: None,
                event_date_provenance: None,
                source_id: Some("src-1".into()),
                byte_start: Some(10),
                byte_end: Some(42),
            }
        );
    }

    #[test]
    fn ingest_fact_renders_attribution_conversation_and_resolved_event_date() {
        let evidence = render_evidence(&ingest_fact_chunk());
        assert_eq!(evidence.author.as_deref(), Some("user"));
        assert_eq!(evidence.conversation_date.as_deref(), Some("2023-03-19"));
        assert_eq!(evidence.event_date.as_deref(), Some("2023-03-01"));
        assert_eq!(evidence.event_date_provenance, Some("resolved"));
        assert_eq!(
            evidence.source_id.as_deref(),
            Some("00000000-0000-4000-8000-000000000001")
        );
        assert_eq!((evidence.byte_start, evidence.byte_end), (None, None));
    }

    #[test]
    fn event_date_without_recorded_cue_is_unknown_never_explicit() {
        let mut chunk = chunk();
        chunk.extraction_meta = Some(meta(Some("2023-03-01"), None, None));
        let evidence = render_evidence(&chunk);
        assert_eq!(evidence.event_date_provenance, Some("unknown"));
        chunk.extraction_meta = Some(meta(Some("2023-03-01"), Some("  "), None));
        assert_eq!(
            render_evidence(&chunk).event_date_provenance,
            Some("unknown")
        );
    }

    #[test]
    fn legacy_chunk_is_all_null_and_serializes_all_seven_keys() {
        let evidence = render_evidence(&chunk());
        assert_eq!(evidence, Evidence::default());
        let value = serde_json::to_value(&evidence).unwrap();
        // `serde_json::Value` objects are key-sorted; compare as a set.
        let mut keys: Vec<_> = value.as_object().unwrap().keys().cloned().collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "author",
                "byte_end",
                "byte_start",
                "conversation_date",
                "event_date",
                "event_date_provenance",
                "source_id",
            ]
        );
        assert!(value.as_object().unwrap().values().all(|v| v.is_null()));
    }

    #[test]
    fn invalid_dates_become_null_and_provenance_follows() {
        for bad in [
            "2023-02-30",
            "2023-3-1",
            "yesterday",
            "",
            "2023-03-01T00:00:00Z",
        ] {
            let mut chunk = fragment_chunk();
            chunk.fragment.as_mut().unwrap().conversation_date = Some(bad.into());
            chunk.extraction_meta = Some(meta(Some(bad), Some("cue"), None));
            let evidence = render_evidence(&chunk);
            assert_eq!(evidence.conversation_date, None, "{bad}");
            assert_eq!(evidence.event_date, None, "{bad}");
            assert_eq!(evidence.event_date_provenance, None, "{bad}");
        }
    }

    #[test]
    fn attribution_names_and_forged_markers_are_not_evidence() {
        let mut chunk = chunk();
        chunk.extraction_meta = Some(meta(None, None, Some("Anna")));
        chunk.metadata = Some(json!({"_loomem_ingest": {
            "version": 1, "kind": "fact", "conversation_date": "2023-03-19"}}));
        let evidence = render_evidence(&chunk);
        assert_eq!(evidence.author, None);
        assert_eq!(evidence.conversation_date, None);
        chunk.extraction_meta = Some(meta(None, None, Some(" Assistant ")));
        assert_eq!(render_evidence(&chunk).author.as_deref(), Some("assistant"));
    }

    #[test]
    fn hit_evidence_follows_the_flag_and_tolerates_an_unreadable_chunk() {
        let chunk = fragment_chunk();
        assert_eq!(hit_evidence(false, Some(&chunk)), None);
        assert_eq!(hit_evidence(false, None), None);
        assert_eq!(hit_evidence(true, None), Some(Evidence::default()));
        assert_eq!(
            hit_evidence(true, Some(&chunk)),
            Some(render_evidence(&chunk))
        );
        assert_eq!(hit_text_suffix(None), "");
    }

    #[test]
    fn text_suffix_omits_null_parts_and_is_empty_when_all_null() {
        assert_eq!(evidence_text_suffix(&Evidence::default()), "");
        assert_eq!(
            evidence_text_suffix(&render_evidence(&fragment_chunk())),
            " [author: assistant; conversation: 2023-03-19; source: src-1#10-42]"
        );
        assert_eq!(
            evidence_text_suffix(&render_evidence(&ingest_fact_chunk())),
            " [author: user; conversation: 2023-03-19; event: 2023-03-01 (resolved); \
             source: 00000000-0000-4000-8000-000000000001]"
        );
        let only_event = Evidence {
            event_date: Some("2023-03-01".into()),
            event_date_provenance: Some("unknown"),
            ..Evidence::default()
        };
        assert_eq!(
            evidence_text_suffix(&only_event),
            " [event: 2023-03-01 (unknown)]"
        );
    }
}
