//! The caller acquires Tantivy before this synchronous lifecycle publication guard.
use super::*;
use crate::tantivy_index::{TantivyIndex, TextDocument};

impl GraphStore {
    /// Rebuild from current sanitized auxiliary rows, never an old model snapshot.
    /// No await occurs while holding the source lifecycle lock.
    pub fn reindex_extraction_if_current(
        &self,
        target: &GraphContributionTarget,
        index: &mut TantivyIndex,
    ) -> Result<()> {
        let _guard = self.store.lock_lifecycle()?;
        let chunk = self
            .store
            .get_chunk(&target.chunk_id)?
            .context("Graph support missing")?;
        ensure!(chunk.deleted_at.is_none(), "Graph support deleted");
        self.validate_contribution_target(Some(target), &chunk)?;
        let entities = self.store.get_entities(&chunk.id, &chunk.stream)?;
        let relations = self.store.get_relations(&chunk.id, &chunk.stream)?;
        let doc = TextDocument {
            id: chunk.id.clone(),
            content: chunk.content.clone(),
            user_id: String::new(),
            app_id: String::new(),
            level: chunk.level,
            timestamp: i64::try_from(chunk.timestamp)
                .context("Graph source timestamp exceeds i64")?,
            stream: chunk.stream.clone(),
            entities: Some(entities.join(",")),
            relations: Some(
                relations
                    .into_iter()
                    .map(|(left, kind, right)| format!("{left} {kind} {right}"))
                    .collect::<Vec<_>>()
                    .join(", "),
            ),
            event_date: legacy_input_event_date(&chunk),
            source_agent: chunk.source.as_ref().map(|tag| tag.agent.clone()),
        };
        index.upsert_chunk_document(doc, &chunk)
    }
}

// Preserve the former extraction writer's legacy noon timestamp. The chunk-aware
// upsert independently applies strict nullable midnight semantics to owned ingest.
fn legacy_input_event_date(chunk: &Chunk) -> Option<i64> {
    let raw = chunk.extraction_meta.as_ref()?.event_date.as_deref()?;
    chrono::NaiveDate::parse_from_str(raw, "%Y-%m-%d")
        .ok()?
        .and_hms_opt(12, 0, 0)
        .map(|date| date.and_utc().timestamp())
}
