//! Explicit fact-only projection; authenticated user/profile fields never enter it.
use super::super::{Chunk, ExtractionMeta, IngestContext, ProvenanceRole};
use loomem_core::{fragments::FragmentMeta, source_tag::SourceTag};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExpectedFact {
    content: String,
    level: i32,
    score: f64,
    timestamp: u64,
    updated_at: Option<u64>,
    consolidated: bool,
    source_ids: Option<Vec<String>>,
    metadata: Option<Value>,
    extraction_meta: Option<ExtractionMeta>,
    source: Option<SourceTag>,
    trust_level: Option<String>,
    provenance_role: ProvenanceRole,
    persistent: bool,
    importance: Option<f64>,
    memory_type: Option<String>,
    valid_from: Option<u64>,
    valid_until: Option<u64>,
    is_latest: bool,
    superseded_by: Option<String>,
    supersedes_id: Option<String>,
    dormant: bool,
    in_progress: bool,
    deleted_at: Option<u64>,
    fragment: Option<FragmentMeta>,
}

impl ExpectedFact {
    pub(super) fn capture(context: &IngestContext<'_>, candidate: &Chunk) -> Option<Self> {
        valid_candidate(context, candidate).then(|| {
            let content = crate::handlers::ingest::sanitize::stored_content(
                &context.state.pii_filter,
                &candidate.content,
            );
            Self::from_chunk(candidate, content)
        })
    }

    fn from_chunk(candidate: &Chunk, content: String) -> Self {
        Self {
            content,
            level: candidate.level,
            score: candidate.score,
            timestamp: candidate.timestamp,
            updated_at: candidate.updated_at,
            consolidated: candidate.consolidated,
            source_ids: candidate.source_ids.clone(),
            metadata: candidate.metadata.clone(),
            extraction_meta: candidate.extraction_meta.clone(),
            source: candidate.source.clone(),
            trust_level: candidate.trust_level.clone(),
            provenance_role: candidate.provenance_role,
            persistent: candidate.persistent,
            importance: candidate.importance,
            memory_type: candidate.memory_type.clone(),
            valid_from: candidate.valid_from,
            valid_until: candidate.valid_until,
            is_latest: candidate.is_latest,
            superseded_by: candidate.superseded_by.clone(),
            supersedes_id: candidate.supersedes_id.clone(),
            dormant: candidate.dormant,
            in_progress: candidate.in_progress,
            deleted_at: candidate.deleted_at,
            fragment: candidate.fragment.clone(),
        }
    }
}

fn valid_candidate(context: &IngestContext<'_>, candidate: &Chunk) -> bool {
    candidate.score.is_finite()
        && candidate.importance.is_none_or(f64::is_finite)
        && candidate.fragment.is_none()
        && candidate.source_ids.as_deref() == Some(std::slice::from_ref(&context.source_id))
        && candidate.extraction_meta.as_ref().is_some_and(|meta| {
            meta.confidence.is_finite()
                && meta.extracted_from.as_deref() == Some(context.source_id.as_str())
        })
}

#[cfg(feature = "evaluation")]
mod readiness;
