//! Atomic RocksDB + Tantivy persist helper for chunk write paths.
//!
//! Extracted from `loomem-server::handlers::ingest::persist_chunk` in cycle /46.
//! Three callsites converge here:
//! - CS1: ingest handler (full intent-log pattern)
//! - CS2: dream consolidation (was RocksDB-only; now writes Tantivy too)
//! - CS3: admin reprocess legacy (result ignored, FIXME(cycle/A))
//!
//! Rollback semantics (cycle /46, refactor-pure):
//! - intent_log entry stays `pending` if Tantivy write fails
//! - intent_log replay at next restart will re-attempt Tantivy write
//! - compensating RocksDB delete is NOT implemented here — see Cycle A

use anyhow::{Context, Result};
use tokio::sync::Mutex;
use tracing::debug;

use crate::intent_log::{IntentLog, OpType};
use crate::storage::RocksDbStore;
use crate::tantivy_index::{TantivyIndex, TextDocument};

use super::Chunk;

/// Arguments for [`persist_chunk_with_index`].
///
/// `intent_log` is `None` for callsites that do not participate in the
/// intent-log protocol (CS2 dream, CS3 admin reprocess).
pub struct PersistChunkArgs<'a> {
    pub chunk: &'a Chunk,
    pub text_doc: TextDocument,
    pub intent_log: Option<&'a Mutex<IntentLog>>,
    pub op: OpType,
}

/// Persist a chunk atomically to RocksDB and Tantivy.
///
/// Steps:
/// 1. `intent_log.append_pending` if `args.intent_log` is `Some`
/// 2. `store.store_chunk` — propagates on failure (nothing to roll back)
/// 3. `tantivy.upsert_document` + `commit` — on failure, intent_log entry
///    stays `pending` (replay on next restart); RocksDB write is NOT reversed
///    (compensating delete is Cycle A scope).  `upsert_document` is used
///    (not `index_document`) to prevent BM25 duplicates on reprocess paths.
/// 4. `intent_log.mark_committed` if both writes succeeded
pub async fn persist_chunk_with_index(
    store: &RocksDbStore,
    tantivy: &Mutex<TantivyIndex>,
    args: PersistChunkArgs<'_>,
) -> Result<()> {
    let chunk_id = &args.chunk.id;

    // Step 1: intent_log pending before cross-store writes
    let intent_seq = if let Some(ilog) = args.intent_log {
        let mut log = ilog.lock().await;
        let seq = log
            .append_pending(args.op, chunk_id)
            .context("intent_log append_pending failed")?;
        Some((ilog, seq))
    } else {
        None
    };

    // Step 2: RocksDB write
    store
        .store_chunk(args.chunk)
        .context("store_chunk failed")?;

    // Step 3: Tantivy write (upsert + commit)
    // upsert_document (delete-then-insert by id) is required for idempotency:
    // on a fresh id it behaves identically to index_document; on a duplicate id
    // (CS3 reprocess, or any retry-with-same-id scenario) it replaces instead
    // of appending a second doc with the same chunk_id.  Using index_document
    // here would silently create BM25 duplicate docs on every reprocess pass
    // (H1 finding, cycle /46 critic).
    {
        let mut tantivy_guard = tantivy.lock().await;
        tantivy_guard
            .upsert_document(args.text_doc)
            .context("tantivy upsert_document failed")?;
        tantivy_guard.commit().context("tantivy commit failed")?;
    }

    debug!(
        "persist_chunk_with_index: chunk {} stored + indexed",
        chunk_id
    );

    // Step 4: intent_log mark_committed (only if both writes succeeded)
    if let Some((ilog, seq)) = intent_seq {
        let mut log = ilog.lock().await;
        log.mark_committed(seq, args.op, chunk_id)
            .context("intent_log mark_committed failed")?;
    }

    Ok(())
}
