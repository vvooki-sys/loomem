//! Access audit (ADR-018, cycle /150e) — records *who* read/searched/wrote
//! *which* memory, *when*. Distinct from the admin-action audit (`audit.rs`,
//! `audit:{user_id}:*`): this is per-**stream** accountability for the data
//! plane (read/search/store), keyed `access:{stream}:{ts:020}:{seq:06}` and
//! encrypted at rest with scope = `stream` (ADR-013 reuse, D8).
//!
//! **Metadata only** (Q3): the record never carries query text or content —
//! only actor identity (from `AuthContext`, no email — D7), operation, target
//! id, and result count. **Write is best-effort** (Q5): a failure increments a
//! counter + warns but never blocks the read/search/store hot path.
//!
//! Default **off** (env-gated, `AccessAuditConfig`); when disabled the handler
//! hooks are no-ops, so behavior is byte-identical to pre-/150e (AC7). The
//! handler hooks themselves land in /150e-2; this module is the storage-facing
//! core (record shape + append + read), fully usable and testable on its own.

pub mod config;
pub use config::AccessAuditConfig;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use tracing::warn;

use crate::storage::RocksDbStore;

/// The data-plane operation that touched memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccessOp {
    Search,
    Store,
    FileRead,
}

/// One access-audit record — **metadata only** (ADR-018 Q3). No query text, no
/// content, no email.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccessRecord {
    /// Principal that performed the access (`AuthContext.user_id`). `None` for
    /// anonymous/legacy callers without a resolved user id.
    pub actor_user_id: Option<String>,
    /// Stream the access targeted (`AuthContext.stream_id`) — also the storage
    /// scope.
    pub stream: String,
    /// Effective role + auth vector, captured as text for forensics.
    pub role: String,
    pub scope: String,
    pub op: AccessOp,
    /// Chunk-id / file-id touched. `None` for `Search` (aggregate — only
    /// `result_count` is recorded; ADR-018 §Konsekwencje).
    pub target_id: Option<String>,
    /// Number of results/chunks touched by the operation.
    pub result_count: usize,
    /// Unix seconds.
    pub ts: u64,
}

/// Current unix seconds — used by the /150e-2 handler hooks to stamp records.
pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Process-wide tie-breaker so records appended within the same unix second
/// keep insertion order (mirrors `audit::SEQ`).
static SEQ: AtomicU32 = AtomicU32::new(0);

/// Process-wide count of access-record write failures (Q5 best-effort signal).
static APPEND_FAILURES: AtomicU64 = AtomicU64::new(0);

/// Number of access records that failed to persist. Non-zero ⇒ the access trail
/// is incomplete.
pub fn record_failure_count() -> u64 {
    APPEND_FAILURES.load(Ordering::Relaxed)
}

/// Persist one access record. **Best-effort (Q5):** a write failure is counted
/// and `warn!`ed and the error is returned, but callers (the read/search/store
/// hooks) ignore it so the hot path is never blocked.
pub fn record(store: &RocksDbStore, rec: &AccessRecord) -> Result<()> {
    let bytes = match serde_json::to_vec(rec).context("failed to serialize AccessRecord") {
        Ok(b) => b,
        Err(e) => {
            // Serialization loss is still a dropped record — count it too
            // (Greptile #253: it previously escaped APPEND_FAILURES via `?`).
            note_record_failure(rec, &e);
            return Err(e);
        }
    };
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let result = store.append_access(&rec.stream, rec.ts, seq, &bytes);
    if let Err(e) = &result {
        note_record_failure(rec, e);
    }
    result
}

/// Count + warn one dropped access record (serialize or write failure), so
/// `record_failure_count` reflects every "could not persist" path (Q5).
fn note_record_failure(rec: &AccessRecord, e: &anyhow::Error) {
    APPEND_FAILURES.fetch_add(1, Ordering::Relaxed);
    warn!(
        target: "audit",
        "access-audit record dropped (stream={}, op={:?}, total_failures={}): {e:#}",
        rec.stream,
        rec.op,
        APPEND_FAILURES.load(Ordering::Relaxed)
    );
}

/// Result of an access-trail read: decrypted records plus a count of rows that
/// could not be surfaced (undecryptable / undeserializable) — same
/// silent-truncation guard as `audit::AuditListing`.
#[derive(Debug, Serialize)]
pub struct AccessListing {
    pub records: Vec<AccessRecord>,
    pub dropped: usize,
}

/// Return up to `limit` access records for `stream`, **newest first**, plus a
/// count of rows that could not be read.
pub fn list_access(store: &RocksDbStore, stream: &str, limit: usize) -> Result<AccessListing> {
    let (raw, mut dropped) = store.scan_access(stream, usize::MAX);
    let mut records: Vec<AccessRecord> = raw
        .iter()
        .filter_map(|bytes| match serde_json::from_slice(bytes) {
            Ok(r) => Some(r),
            Err(e) => {
                tracing::error!("failed to deserialize AccessRecord: {e}");
                dropped += 1;
                None
            }
        })
        .collect();
    records.reverse();
    records.truncate(limit);
    Ok(AccessListing { records, dropped })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RocksDbConfig;
    use tempfile::TempDir;

    fn test_config() -> RocksDbConfig {
        RocksDbConfig {
            max_open_files: 100,
            compression: "lz4".to_string(),
            write_buffer_size: 4 * 1024 * 1024,
            max_write_buffer_number: 2,
        }
    }

    fn rec(op: AccessOp, target: Option<&str>, ts: u64) -> AccessRecord {
        AccessRecord {
            actor_user_id: Some("u-actor".into()),
            stream: "s1".into(),
            role: "Member".into(),
            scope: "Private".into(),
            op,
            target_id: target.map(|s| s.to_string()),
            result_count: 3,
            ts,
        }
    }

    #[test]
    fn record_and_list_newest_first() -> Result<()> {
        let temp = TempDir::new()?;
        let store = RocksDbStore::open(temp.path(), &test_config())?;
        record(&store, &rec(AccessOp::Search, None, 100))?;
        record(&store, &rec(AccessOp::Store, Some("c1"), 200))?;
        record(&store, &rec(AccessOp::FileRead, Some("f1"), 300))?;

        let listing = list_access(&store, "s1", 10)?;
        assert_eq!(listing.dropped, 0);
        assert_eq!(listing.records.len(), 3);
        assert_eq!(listing.records[0].ts, 300);
        assert_eq!(listing.records[0].op, AccessOp::FileRead);
        assert_eq!(listing.records[2].ts, 100);
        Ok(())
    }

    #[test]
    fn list_respects_limit() -> Result<()> {
        let temp = TempDir::new()?;
        let store = RocksDbStore::open(temp.path(), &test_config())?;
        for ts in 0..10 {
            record(&store, &rec(AccessOp::Search, None, ts))?;
        }
        let listing = list_access(&store, "s1", 3)?;
        assert_eq!(listing.records.len(), 3);
        assert_eq!(listing.records[0].ts, 9);
        Ok(())
    }

    #[test]
    fn list_isolated_per_stream() -> Result<()> {
        let temp = TempDir::new()?;
        let store = RocksDbStore::open(temp.path(), &test_config())?;
        let mut r = rec(AccessOp::Search, None, 100);
        record(&store, &r)?;
        r.stream = "s2".into();
        record(&store, &r)?;

        assert_eq!(list_access(&store, "s1", 10)?.records.len(), 1);
        assert_eq!(list_access(&store, "s2", 10)?.records.len(), 1);
        Ok(())
    }

    #[test]
    fn list_counts_undeserializable_rows() -> Result<()> {
        let temp = TempDir::new()?;
        let store = RocksDbStore::open(temp.path(), &test_config())?;
        record(&store, &rec(AccessOp::Search, None, 100))?;
        store.append_access("s1", 200, 0, b"{not valid json")?;

        let listing = list_access(&store, "s1", 10)?;
        assert_eq!(listing.records.len(), 1);
        assert_eq!(listing.dropped, 1);
        Ok(())
    }

    #[test]
    fn record_roundtrips_metadata_only() {
        // ADR-018 Q3: the wire shape carries no query/content fields.
        let r = rec(AccessOp::Search, None, 1);
        let json = serde_json::to_string(&r).unwrap();
        assert!(
            !json.contains("query"),
            "AccessRecord must not carry query text"
        );
        assert!(!json.contains("email"), "AccessRecord must not carry email");
        let back: AccessRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(back, r);
    }
}
