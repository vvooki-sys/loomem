//! Pure expectations for the owned, immutable evaluation harness. Never reads storage.
use std::collections::HashSet;

use anyhow::{ensure, Context};
use loomem_core::fragments::{segment_source, CanonicalSource, FragmentMeta};
use loomem_core::storage::Chunk;
use serde::Deserialize;

use super::{fragments, SANITIZE_MAX_PASSES};
use crate::AppState;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SourceExpectation {
    pub content: String,
    pub conversation_date: Option<String>,
    pub source_id: String,
    pub fragment_ids: Vec<String>,
}

#[derive(Debug)]
pub(crate) struct ExpectedRecord {
    pub id: String,
    pub content: String,
    pub fragment: Option<FragmentMeta>,
}

/// Reconstruct the complete source/child set from the original ingest arguments.
/// Receipt completeness must also be checked by the caller; sparse/omitted IDs
/// never authorize silently dropping a derived fragment.
pub(crate) fn reconstruct(
    state: &AppState,
    request: &SourceExpectation,
) -> anyhow::Result<Vec<ExpectedRecord>> {
    ensure!(state.config.fragments.enabled, "fragmentation is disabled");
    let source_uuid = uuid::Uuid::parse_str(&request.source_id).context("invalid source ID")?;
    ensure!(
        source_uuid.to_string() == request.source_id,
        "noncanonical source ID"
    );
    validate_date(request.conversation_date.as_deref())?;
    let content = state
        .pii_filter
        .redact_for_sink_fixpoint(&request.content, SANITIZE_MAX_PASSES)
        .context("source sanitization did not stabilize")?;
    let source = CanonicalSource {
        id: request.source_id.clone(),
        text: content,
        conversation_date: request.conversation_date.clone(),
    };
    let counter = fragments::token_counter(state).map_err(anyhow::Error::msg)?;
    let segmented = segment_source(&source, &state.config.fragments, counter.as_ref())?;
    let mut records = vec![ExpectedRecord {
        id: source.id.clone(),
        content: source.text.clone(),
        fragment: None,
    }];
    for fragment in segmented.iter().filter(|f| !f.text.trim().is_empty()) {
        let aligned = fragments::align_to_sink(&state.pii_filter, fragment)
            .context("fragment would be skipped by sink; partial reconstruction refused")?;
        ensure!(
            source.text.get(aligned.byte_start..aligned.byte_end) == Some(aligned.text.as_str()),
            "fragment range does not match source"
        );
        records.push(ExpectedRecord {
            id: aligned.id.clone(),
            content: aligned.text.clone(),
            fragment: Some(FragmentMeta::from(&aligned)),
        });
    }
    let receipt: HashSet<_> = request.fragment_ids.iter().collect();
    ensure!(
        receipt.len() == request.fragment_ids.len(),
        "duplicate receipt child"
    );
    let derived: HashSet<_> = records.iter().skip(1).map(|r| &r.id).collect();
    ensure!(
        receipt == derived,
        "receipt does not match complete derived fragment set"
    );
    Ok(records)
}

fn validate_date(date: Option<&str>) -> anyhow::Result<()> {
    if let Some(date) = date {
        let parsed = chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d")?;
        ensure!(
            parsed.format("%Y-%m-%d").to_string() == date,
            "noncanonical date"
        );
    }
    Ok(())
}

/// Verify persisted provenance against independent expectations. Content,
/// stream, liveness, vectors and index visibility are checked by the core probe.
pub(crate) fn metadata_matches(
    expected: &ExpectedRecord,
    chunk: &Chunk,
    conversation_date: Option<&str>,
) -> bool {
    let kind = if expected.fragment.is_some() {
        "fragment"
    } else {
        "source"
    };
    let marker = serde_json::json!({"version": 1, "kind": kind,
        "conversation_date": conversation_date});
    let source_ids_match = match &expected.fragment {
        Some(meta) => chunk.source_ids.as_deref() == Some(std::slice::from_ref(&meta.source_id)),
        None => chunk.source_ids.is_none(),
    };
    chunk.id == expected.id
        && chunk.fragment == expected.fragment
        && source_ids_match
        && chunk
            .metadata
            .as_ref()
            .and_then(|m| m.get("_loomem_ingest"))
            == Some(&marker)
}

#[cfg(test)]
mod tests;
