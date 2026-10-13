use super::{IntentEntry, IntentLog, OpType, RecoveryReport};
use crate::{storage::RocksDbStore, tantivy_index::TantivyIndex};
use anyhow::{Context, Result};
use tracing::info;

/// Replay a single chunk's Tantivy state from RocksDB. Used by recover()
/// for both Store and Consolidate replay paths.
///
/// Returns Ok(true) if the chunk existed in RocksDB and was re-indexed,
/// Ok(false) if missing or tombstoned; stale index entries are deleted.
fn replay_chunk_to_tantivy(
    store: &crate::storage::RocksDbStore,
    tantivy: &mut crate::tantivy_index::TantivyIndex,
    chunk_id: &str,
) -> Result<bool> {
    match store.get_chunk(chunk_id)? {
        Some(chunk) if chunk.deleted_at.is_none() => {
            let doc = build_recovery_text_doc(store, &chunk)?;
            tantivy.upsert_document(doc)?;
            Ok(true)
        }
        _ => {
            tantivy.delete_document(chunk_id)?;
            Ok(false)
        }
    }
}

/// Build a Tantivy TextDocument for a chunk, fetching entities/relations from RocksDB.
/// Used by recover() for both Store and Consolidate replay paths.
fn build_recovery_text_doc(
    store: &crate::storage::RocksDbStore,
    chunk: &crate::storage::Chunk,
) -> Result<crate::tantivy_index::TextDocument> {
    let entities = store.get_entities(&chunk.id, &chunk.stream)?;
    let relations = store.get_relations(&chunk.id, &chunk.stream)?;

    let entity_text = entities.join(" ");
    let relation_text = relations
        .iter()
        .map(|(s, r, o)| format!("{} {} {}", s, r, o))
        .collect::<Vec<_>>()
        .join(" ");

    let event_date_ts: Option<i64> = chunk
        .extraction_meta
        .as_ref()
        .and_then(|m| m.event_date.as_ref())
        .and_then(|d| chrono::NaiveDate::parse_from_str(d, "%Y-%m-%d").ok())
        .and_then(|d| d.and_hms_opt(12, 0, 0))
        .map(|dt| dt.and_utc().timestamp());

    Ok(crate::tantivy_index::TextDocument {
        id: chunk.id.clone(),
        content: chunk.content.clone(),
        user_id: String::new(),
        app_id: String::new(),
        level: chunk.level,
        stream: chunk.stream.clone(),
        timestamp: i64::try_from(chunk.timestamp).context("Recovery timestamp exceeds i64")?,
        entities: Some(entity_text),
        relations: Some(relation_text),
        event_date: event_date_ts,
        source_agent: chunk.source.as_ref().map(|s| s.agent.clone()),
    })
}

/// Replay pending intents, committing the derived index before acknowledging any intent.
pub fn recover(
    intent_log: &mut IntentLog,
    store: &RocksDbStore,
    tantivy: &mut TantivyIndex,
) -> Result<RecoveryReport> {
    recover_with_commit(intent_log, store, tantivy, TantivyIndex::commit)
}

// A commit boundary seam lets tests simulate an I/O failure without replacing either store.
fn recover_with_commit(
    intent_log: &mut IntentLog,
    store: &RocksDbStore,
    tantivy: &mut TantivyIndex,
    commit: impl FnOnce(&mut TantivyIndex) -> Result<()>,
) -> Result<RecoveryReport> {
    let pending = intent_log
        .scan_pending()
        .context("Failed to scan recovery intents")?;
    let mut report = RecoveryReport::default();
    if pending.is_empty() {
        return Ok(report);
    }
    for entry in &pending {
        if replay_entry(entry, store, tantivy)? {
            report.replayed += 1;
        } else {
            report.skipped += 1;
        }
    }
    commit(tantivy).context("Failed to commit recovered index; intents remain pending")?;
    for entry in &pending {
        intent_log.mark_committed(entry.seq, entry.op, &entry.id)?;
    }
    info!(
        "Intent log recovery complete: {} replayed, {} skipped",
        report.replayed, report.skipped
    );
    Ok(report)
}

fn replay_entry(
    entry: &IntentEntry,
    store: &RocksDbStore,
    tantivy: &mut TantivyIndex,
) -> Result<bool> {
    match entry.op {
        OpType::Store | OpType::Consolidate => replay_chunk_to_tantivy(store, tantivy, &entry.id),
        OpType::Delete => {
            if store.get_chunk(&entry.id)?.is_some() {
                store.delete_by_id(&entry.id)?;
            }
            tantivy.delete_document(&entry.id)?;
            Ok(true)
        }
        OpType::PurgeChunk => {
            store.hard_delete_by_id(&entry.id)?;
            tantivy.delete_document(&entry.id)?;
            Ok(true)
        }
        OpType::Purge => {
            store.purge_namespace(&entry.id, false)?;
            tantivy.delete_stream(&entry.id)?;
            Ok(true)
        }
    }
}

#[cfg(test)]
pub(super) fn recover_with_failed_commit(
    intent_log: &mut IntentLog,
    store: &RocksDbStore,
    tantivy: &mut TantivyIndex,
) -> Result<RecoveryReport> {
    recover_with_commit(intent_log, store, tantivy, |_| {
        anyhow::bail!("injected commit failure")
    })
}
