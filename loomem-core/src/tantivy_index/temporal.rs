//! Event time is nullable for versioned ingest; legacy index callers retain
//! recorded-time fallback. Uses existing fields, with no schema migration.
use super::{TantivyIndex, TextDocument};
use crate::storage::{
    is_ingest_fact_shape, is_ingest_fragment, is_ingest_source, Chunk, RocksDbStore,
};
use anyhow::{ensure, Context, Result};
use tantivy::{doc, Term};

/// Source, fact and fragment: a fragment carries no `extraction_meta`, so
/// its event time is always unknown — never the storage timestamp.
fn owned(chunk: &Chunk) -> bool {
    is_ingest_source(chunk) || is_ingest_fact_shape(chunk) || is_ingest_fragment(chunk)
}

/// Authoritative event time for recognized versioned ingest only. Missing or
/// invalid dates remain unknown; source observation/recording time is not event time.
#[must_use]
pub fn event_timestamp_for_chunk(chunk: &Chunk) -> Option<i64> {
    if !owned(chunk) {
        return None;
    }
    let raw = chunk.extraction_meta.as_ref()?.event_date.as_deref()?;
    let date = chrono::NaiveDate::parse_from_str(raw, "%Y-%m-%d").ok()?;
    if date.format("%Y-%m-%d").to_string() != raw {
        return None;
    }
    Some(date.and_hms_opt(0, 0, 0)?.and_utc().timestamp())
}

fn bind_document(doc: &TextDocument, chunk: &Chunk) -> Result<()> {
    ensure!(
        doc.id == chunk.id && doc.stream == chunk.stream,
        "index document/chunk identity mismatch"
    );
    Ok(())
}

impl TantivyIndex {
    /// Legacy upsert preserves recorded-time fallback for existing callers.
    pub fn upsert_document(&mut self, doc: TextDocument) -> Result<()> {
        self.writer
            .delete_term(Term::from_field_text(self.id_field, &doc.id));
        self.index_document(doc)
    }

    /// Low-level legacy writer. Durable chunk writers use index_chunk_document.
    pub fn index_document(&mut self, doc: TextDocument) -> Result<()> {
        let date = Some(doc.event_date.unwrap_or(doc.timestamp));
        self.write_temporal_document(doc, date)
    }

    /// Upsert authoritative chunk state without inventing an unknown event time.
    /// A tombstone removes its old index document instead of reintroducing it.
    pub fn upsert_chunk_document(&mut self, doc: TextDocument, chunk: &Chunk) -> Result<()> {
        bind_document(&doc, chunk)?;
        self.writer
            .delete_term(Term::from_field_text(self.id_field, &doc.id));
        self.index_chunk_document(doc, chunk)
    }

    /// Index a chunk with strict nullable event semantics only for recognized
    /// versioned ingest. Known pre-epoch event dates remain valid i64 dates.
    pub fn index_chunk_document(&mut self, doc: TextDocument, chunk: &Chunk) -> Result<()> {
        bind_document(&doc, chunk)?;
        if chunk.deleted_at.is_some() {
            self.writer
                .delete_term(Term::from_field_text(self.id_field, &doc.id));
            return Ok(());
        }
        let date = if owned(chunk) {
            event_timestamp_for_chunk(chunk)
        } else {
            Some(doc.event_date.unwrap_or(doc.timestamp))
        };
        self.write_temporal_document(doc, date)
    }

    fn write_temporal_document(&mut self, doc: TextDocument, event: Option<i64>) -> Result<()> {
        let mut document = doc!(
            self.id_field => doc.id, self.content_field => doc.content,
            self.user_id_field => doc.user_id, self.app_id_field => doc.app_id,
            self.level_field => i64::from(doc.level), self.timestamp_field => doc.timestamp,
            self.stream_field => doc.stream,
        );
        if let Some(date) = event {
            document.add_i64(self.event_date_field, date);
        }
        if let Some(entities) = doc.entities {
            document.add_text(self.entities_field, entities);
        }
        if let Some(relations) = doc.relations {
            document.add_text(self.relations_field, relations);
        }
        document.add_text(
            self.source_agent_field,
            doc.source_agent.as_deref().unwrap_or("unknown"),
        );
        self.writer
            .add_document(document)
            .context("Failed to add document to Tantivy")?;
        Ok(())
    }

    /// Startup-only, idempotent repair for documents written before nullable
    /// versioned dates. Strict RocksDB snapshot; unrelated legacy docs remain
    /// untouched. Commit failure must prevent serving potentially stale dates.
    pub fn repair_ingest_event_dates(&mut self, store: &RocksDbStore) -> Result<usize> {
        let chunks = store.ingest_chunks_for_index_repair()?;
        let count = chunks.len();
        for chunk in chunks {
            if chunk.deleted_at.is_some() {
                self.writer
                    .delete_term(Term::from_field_text(self.id_field, &chunk.id));
                continue;
            }
            let doc = repair_document(store, &chunk)?;
            self.upsert_chunk_document(doc, &chunk)?;
        }
        self.commit()
            .context("commit versioned event-date index repair")?;
        Ok(count)
    }
}

fn repair_document(store: &RocksDbStore, chunk: &Chunk) -> Result<TextDocument> {
    let entities = store.get_entities(&chunk.id, &chunk.stream)?;
    let relations = store.get_relations(&chunk.id, &chunk.stream)?;
    Ok(TextDocument {
        id: chunk.id.clone(),
        content: chunk.content.clone(),
        user_id: "default".into(),
        app_id: "default".into(),
        level: chunk.level,
        timestamp: i64::try_from(chunk.timestamp).context("index repair timestamp exceeds i64")?,
        stream: chunk.stream.clone(),
        entities: Some(entities.join(",")),
        relations: Some(
            relations
                .into_iter()
                .map(|(s, r, o)| format!("{s} {r} {o}"))
                .collect::<Vec<_>>()
                .join(", "),
        ),
        event_date: None,
        source_agent: chunk.source.as_ref().map(|tag| tag.agent.clone()),
    })
}

#[cfg(test)]
mod tests;
