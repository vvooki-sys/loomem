//! Parse the exact receipt whitelist and compare it without sanitizing it again.
use super::*;
use anyhow::{ensure, Result};

impl ExpectedFact {
    pub(crate) fn parse(value: Value, source_id: &str, date: Option<&str>) -> Result<Self> {
        let expected: Self = serde_json::from_value(value.clone())?;
        // Also reject unknown nested fields and omitted nullable fields. The
        // receipt's wire form, not the wider persisted Chunk schema, is accepted.
        ensure!(
            serde_json::to_value(&expected)? == value,
            "noncanonical fact projection"
        );
        ensure!(
            expected.valid_identity(source_id, date),
            "invalid fact provenance"
        );
        ensure!(expected.valid_lifecycle(), "invalid fresh fact lifecycle");
        ensure!(expected.valid_ranking(), "invalid fact ranking");
        Ok(expected)
    }

    pub(crate) fn content(&self) -> &str {
        &self.content
    }

    /// Content is checked separately by the core probe so its status remains
    /// content_mismatch. Every other whitelisted field must match exactly.
    pub(crate) fn metadata_matches(&self, stored: &Chunk) -> bool {
        let observed = Self::from_chunk(stored, self.content.clone());
        match (serde_json::to_value(self), serde_json::to_value(observed)) {
            (Ok(expected), Ok(actual)) => expected == actual,
            _ => false,
        }
    }

    fn valid_identity(&self, source_id: &str, date: Option<&str>) -> bool {
        let marker = serde_json::json!({"_loomem_ingest": {
            "version":1,"kind":"fact","conversation_date":date}});
        self.source_ids.as_deref() == Some(&[source_id.to_owned()][..])
            && self.metadata.as_ref() == Some(&marker)
            && self.source == Some(SourceTag::from_agent("mcp-ingest"))
            && self.trust_level.as_deref() == Some("a2")
            && self.provenance_role == ProvenanceRole::Claim
            && self.extraction_meta.as_ref().is_some_and(|meta| {
                meta.extracted_from.as_deref() == Some(source_id)
                    && meta.confidence.is_finite()
                    && self.valid_from == meta.event_date_unix()
            })
    }

    fn valid_lifecycle(&self) -> bool {
        self.is_latest
            && self.persistent
            && !self.consolidated
            && !self.dormant
            && !self.in_progress
            && self.updated_at == Some(self.timestamp)
            && [self.valid_until, self.deleted_at]
                .iter()
                .all(Option::is_none)
            && [self.superseded_by.as_ref(), self.supersedes_id.as_ref()]
                .iter()
                .all(Option::is_none)
            && self.fragment.is_none()
    }

    fn valid_ranking(&self) -> bool {
        self.level == 1
            && self.score == 1.0
            && self.importance.is_some_and(f64::is_finite)
            && matches!(self.memory_type.as_deref(), Some("static" | "dynamic"))
    }
}
