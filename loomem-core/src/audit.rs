//! Admin-action audit log (cycle B2).
//!
//! Append-only event store scoped per target user. Events are persisted via
//! `RocksDbStore::append_audit` under the `audit:{user_id}:...` key prefix.
//! The storage layer is schema-agnostic; this module defines the wire shape
//! (`AuditEvent`) and the helpers that admin handlers call.
//!
//! Tracked actions (locked decision Q5 in B2-brief): `user_create`,
//! `user_update`, `user_delete`, `force_logout`, `private_stream_toggle`,
//! `api_key_rotate`, `memory_purge` (cycle/135 GDPR Art 17 hard-delete
//! on demand). Retention: infinity (no cleanup job). UI shows last 50.
//!
//! Append failures are **not fatal**: admin handlers log a warning and return
//! success anyway (the admin action already committed; audit loss is a
//! best-effort signal). See B2-brief §Risks.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use crate::storage::RocksDbStore;

/// One audit record. `action` is a short kebab-case verb (`force_logout`,
/// `user_update`, ...). `details` is an open-ended JSON object — callers put
/// whatever context is useful (session counts, field diffs, new role).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEvent {
    pub action: String,
    pub actor_id: String,
    pub actor_email: String,
    pub timestamp: u64,
    pub details: serde_json::Value,
    /// /150d tamper-evidence: hex sha256 of the **previous** audit event's
    /// stored bytes for the same `target_user_id`. `None` for the first event
    /// in a user's chain and for legacy rows written before /150d (they
    /// deserialize to `None` via `#[serde(default)]`). Set by `append`, not by
    /// callers. See `verify_chain`.
    #[serde(default)]
    pub prev_hash: Option<String>,
}

/// Conventional actor ID for events emitted by background tasks (cleanup
/// loops, auto-expiry). See B2-brief §Gotcha #7.
pub const SYSTEM_ACTOR_ID: &str = "system";
pub const SYSTEM_ACTOR_EMAIL: &str = "system@loomem.ai";

impl AuditEvent {
    /// Build an event with `timestamp` = current unix seconds.
    pub fn new(
        action: impl Into<String>,
        actor_id: impl Into<String>,
        actor_email: impl Into<String>,
        details: serde_json::Value,
    ) -> Self {
        Self {
            action: action.into(),
            actor_id: actor_id.into(),
            actor_email: actor_email.into(),
            timestamp: now_unix(),
            details,
            prev_hash: None,
        }
    }

    /// Convenience constructor for background/system-initiated events.
    pub fn system(action: impl Into<String>, details: serde_json::Value) -> Self {
        Self::new(action, SYSTEM_ACTOR_ID, SYSTEM_ACTOR_EMAIL, details)
    }
}

/// Process-wide counter so events appended within the same unix second still
/// order deterministically. Wraps at u32::MAX — in practice a single admin
/// can never hit 4B events in one second.
static SEQ: AtomicU32 = AtomicU32::new(0);

/// Process-wide count of audit-write failures (the `append_audit` store write
/// returned `Err`). /150b Gap 6: the `Err` is propagated and every call site
/// already logs a warning, but the loss had no aggregate signal — this counter
/// gives one regardless of which caller swallowed the error.
static APPEND_FAILURES: AtomicU64 = AtomicU64::new(0);

/// Number of audit events that failed to persist — **for any reason** (write
/// error or serialization failure). Non-zero means the audit log is
/// **incomplete** for forensic purposes.
pub fn append_failure_count() -> u64 {
    APPEND_FAILURES.load(Ordering::Relaxed)
}

/// Count + escalate one append failure (Q2: forensic-integrity event → error).
/// Shared by the serialize and the store-write failure paths so every "could
/// not persist" outcome bumps `APPEND_FAILURES` (Greptile #248).
fn note_append_failure(action: &str, e: &anyhow::Error) {
    APPEND_FAILURES.fetch_add(1, Ordering::Relaxed);
    tracing::error!(
        target: "audit",
        "audit append FAILED (action={action}, total_failures={}): {e:#}",
        APPEND_FAILURES.load(Ordering::Relaxed)
    );
}

/// Hex sha256 of a stored audit blob — the chain link primitive. Hashing the
/// stored plaintext bytes directly (not a re-serialization) keeps append and
/// `verify_chain` byte-exact without relying on canonical-serialization.
fn blob_hash(blob: &[u8]) -> String {
    // 64 lowercase hex chars (32 bytes × 2) — mirrors `crypto::at_rest::index_token`.
    Sha256::digest(blob)
        .iter()
        .fold(String::with_capacity(64), |mut s, b| {
            use std::fmt::Write as _;
            let _ = write!(s, "{b:02x}");
            s
        })
}

/// Append one event to the audit log for `target_user_id`. Links it to the
/// previous event via `prev_hash` (sha256 of the predecessor's stored bytes —
/// /150d tamper-evidence), serializes to JSON, and hands raw bytes to
/// `RocksDbStore::append_audit` (which encrypts at rest).
///
/// A write failure is **non-fatal** (callers log and continue — see module
/// docs, Q2) but is escalated to `error!` and counted (`append_failure_count`)
/// because a missing audit record is a forensic-integrity gap, not noise.
pub fn append(store: &RocksDbStore, target_user_id: &str, event: &AuditEvent) -> Result<()> {
    // Chain to the predecessor's stored bytes. `None` ⇒ first event for this
    // user (or the previous row was undecryptable — then the chain restarts and
    // `verify_chain` will flag the gap).
    let mut event = event.clone();
    event.prev_hash = store.last_audit(target_user_id).map(|b| blob_hash(&b));

    let bytes = match serde_json::to_vec(&event).context("failed to serialize AuditEvent") {
        Ok(b) => b,
        Err(e) => {
            note_append_failure(&event.action, &e);
            return Err(e);
        }
    };
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let result = store.append_audit(target_user_id, event.timestamp, seq, &bytes);
    if let Err(e) = &result {
        note_append_failure(&event.action, e);
    }
    result
}

/// Outcome of a tamper-evidence chain check (`verify_chain`).
#[derive(Debug, PartialEq, Eq)]
pub enum ChainStatus {
    /// Every chained event's `prev_hash` matched its predecessor's stored bytes.
    /// `chained` counts events that participated in the chain (carried `Some`).
    Valid { chained: usize },
    /// A break was found: `index` is the position (in ascending order) whose
    /// `prev_hash` did not match, `reason` explains how.
    Broken { index: usize, reason: String },
    /// Verification could not run reliably: `undecryptable` rows were dropped by
    /// `scan_audit` (DEK loss / corruption), so predecessor blobs are missing
    /// from the scanned slice and a `prev_hash` mismatch cannot be distinguished
    /// from real tampering. Resolve the decryption failure before trusting a
    /// verdict (Greptile #251 — avoids misreporting decrypt-loss as `Broken`).
    Indeterminate { undecryptable: usize },
}

/// Verify the tamper-evidence hash-chain for `target_user_id`.
///
/// Reads all stored events (ascending) and checks each chained event's
/// `prev_hash` against sha256 of its predecessor's stored bytes. Legacy rows
/// (`prev_hash == None`) before the first chained row are skipped (backward
/// compat); once the chain has started, a `None` or mismatching `prev_hash` is
/// a break (deletion/edit/reorder).
///
/// If any stored row failed to decrypt (`scan_audit` `dropped > 0`), the result
/// is [`ChainStatus::Indeterminate`] rather than `Broken`: a missing predecessor
/// blob would make the next row's `prev_hash` mismatch and be misattributed to
/// tampering. The chain is only checked when every row decrypted cleanly.
pub fn verify_chain(store: &RocksDbStore, target_user_id: &str) -> ChainStatus {
    let (blobs, dropped) = store.scan_audit(target_user_id, usize::MAX);
    if dropped > 0 {
        return ChainStatus::Indeterminate {
            undecryptable: dropped,
        };
    }
    verify_chain_blobs(&blobs)
}

/// Pure chain check over stored blobs in ascending order — storage-free so it
/// is deterministically unit-testable.
fn verify_chain_blobs(blobs: &[Vec<u8>]) -> ChainStatus {
    let mut entered = false; // have we passed the first chained (Some) row?
    let mut chained = 0usize;
    for i in 1..blobs.len() {
        let cur: AuditEvent = match serde_json::from_slice(&blobs[i]) {
            Ok(ev) => ev,
            Err(e) => {
                return ChainStatus::Broken {
                    index: i,
                    reason: format!("row {i} failed to deserialize: {e}"),
                }
            }
        };
        match cur.prev_hash {
            None if entered => {
                return ChainStatus::Broken {
                    index: i,
                    reason: format!("row {i} has no prev_hash but the chain had started"),
                }
            }
            None => {} // still in the legacy prefix — skip
            Some(ref h) => {
                entered = true;
                chained += 1;
                let expected = blob_hash(&blobs[i - 1]);
                if h != &expected {
                    return ChainStatus::Broken {
                        index: i,
                        reason: format!("row {i} prev_hash mismatch (chain edited/truncated)"),
                    };
                }
            }
        }
    }
    ChainStatus::Valid { chained }
}

/// Result of an audit-log read: the decrypted, deserialized events plus the
/// number of stored rows that were scanned but could not be surfaced (failed
/// decryption or deserialization). A non-zero `dropped` means the log is
/// **incomplete** — callers must be able to tell that apart from an empty log.
#[derive(Debug, Serialize)]
pub struct AuditListing {
    pub events: Vec<AuditEvent>,
    pub dropped: usize,
}

/// Return up to `limit` events for `target_user_id`, **newest first**, together
/// with a count of rows that could not be read.
///
/// Storage scans return ascending order; we materialize then reverse so the
/// UI gets most-recent-first with no client-side sort. `dropped` aggregates
/// undecryptable rows (from `scan_audit`) and rows that fail JSON
/// deserialization here — both are silent-truncation hazards for an audit log.
pub fn list(store: &RocksDbStore, target_user_id: &str, limit: usize) -> Result<AuditListing> {
    let (raw, mut dropped) = store.scan_audit(target_user_id, usize::MAX);
    let mut events: Vec<AuditEvent> = raw
        .iter()
        .filter_map(|bytes| match serde_json::from_slice(bytes) {
            Ok(ev) => Some(ev),
            Err(e) => {
                tracing::error!("failed to deserialize AuditEvent: {e}");
                dropped += 1;
                None
            }
        })
        .collect();
    events.reverse();
    events.truncate(limit);
    Ok(AuditListing { events, dropped })
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
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

    fn ev(action: &str, ts: u64) -> AuditEvent {
        AuditEvent {
            action: action.into(),
            actor_id: "admin_1".into(),
            actor_email: "admin@loomem.ai".into(),
            timestamp: ts,
            details: serde_json::json!({"x": 1}),
            prev_hash: None,
        }
    }

    #[test]
    fn test_append_and_list_newest_first() -> Result<()> {
        let temp = TempDir::new()?;
        let store = RocksDbStore::open(temp.path(), &test_config())?;

        append(&store, "u1", &ev("a", 100))?;
        append(&store, "u1", &ev("b", 200))?;
        append(&store, "u1", &ev("c", 300))?;

        let events = list(&store, "u1", 10)?.events;
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].action, "c");
        assert_eq!(events[1].action, "b");
        assert_eq!(events[2].action, "a");
        Ok(())
    }

    #[test]
    fn test_list_respects_limit() -> Result<()> {
        let temp = TempDir::new()?;
        let store = RocksDbStore::open(temp.path(), &test_config())?;

        for ts in 0..10 {
            append(&store, "u1", &ev("any", ts))?;
        }

        let events = list(&store, "u1", 3)?.events;
        assert_eq!(events.len(), 3);
        // Newest first → ts 9, 8, 7.
        assert_eq!(events[0].timestamp, 9);
        assert_eq!(events[1].timestamp, 8);
        assert_eq!(events[2].timestamp, 7);
        Ok(())
    }

    #[test]
    fn test_list_isolated_per_user() -> Result<()> {
        let temp = TempDir::new()?;
        let store = RocksDbStore::open(temp.path(), &test_config())?;

        append(&store, "u1", &ev("for_u1", 100))?;
        append(&store, "u2", &ev("for_u2", 200))?;

        let u1 = list(&store, "u1", 10)?.events;
        let u2 = list(&store, "u2", 10)?.events;

        assert_eq!(u1.len(), 1);
        assert_eq!(u1[0].action, "for_u1");
        assert_eq!(u2.len(), 1);
        assert_eq!(u2[0].action, "for_u2");
        Ok(())
    }

    #[test]
    fn test_list_returns_empty_for_unknown_user() -> Result<()> {
        let temp = TempDir::new()?;
        let store = RocksDbStore::open(temp.path(), &test_config())?;
        let listing = list(&store, "u_missing", 10)?;
        assert!(listing.events.is_empty());
        assert_eq!(listing.dropped, 0);
        Ok(())
    }

    #[test]
    fn test_same_second_events_keep_insertion_order() -> Result<()> {
        let temp = TempDir::new()?;
        let store = RocksDbStore::open(temp.path(), &test_config())?;

        // Three events at the same timestamp — seq counter disambiguates.
        append(&store, "u1", &ev("first", 500))?;
        append(&store, "u1", &ev("second", 500))?;
        append(&store, "u1", &ev("third", 500))?;

        let events = list(&store, "u1", 10)?.events;
        assert_eq!(events.len(), 3);
        // Newest first = last appended first.
        assert_eq!(events[0].action, "third");
        assert_eq!(events[1].action, "second");
        assert_eq!(events[2].action, "first");
        Ok(())
    }

    #[test]
    fn test_list_counts_undeserializable_rows() -> Result<()> {
        let temp = TempDir::new()?;
        let store = RocksDbStore::open(temp.path(), &test_config())?;

        // One valid event plus one row of non-JSON bytes written straight to
        // the audit keyspace (NoopProvider = stored plaintext). The garbage row
        // must be reported as dropped, not silently vanish.
        append(&store, "u1", &ev("ok", 100))?;
        store.append_audit("u1", 200, 0, b"{not valid json")?;

        let listing = list(&store, "u1", 10)?;
        assert_eq!(listing.events.len(), 1);
        assert_eq!(listing.events[0].action, "ok");
        assert_eq!(listing.dropped, 1);
        Ok(())
    }

    #[test]
    fn test_system_helper_sets_conventional_actor() {
        let e = AuditEvent::system("cleanup", serde_json::json!({"n": 3}));
        assert_eq!(e.actor_id, SYSTEM_ACTOR_ID);
        assert_eq!(e.actor_email, SYSTEM_ACTOR_EMAIL);
        assert_eq!(e.action, "cleanup");
    }

    // ── /150d tamper-evidence hash-chain ───────────────────────────────

    /// Build a valid chain of stored blobs: each event's `prev_hash` is the
    /// sha256 of the previous stored blob (first event = None).
    fn chained_blobs(actions: &[&str]) -> Vec<Vec<u8>> {
        let mut blobs: Vec<Vec<u8>> = Vec::new();
        let mut prev: Option<String> = None;
        for (i, a) in actions.iter().enumerate() {
            let mut e = ev(a, 100 + i as u64);
            e.prev_hash = prev.clone();
            let b = serde_json::to_vec(&e).expect("serialize");
            prev = Some(blob_hash(&b));
            blobs.push(b);
        }
        blobs
    }

    #[test]
    fn test_append_chains_prev_hash() -> Result<()> {
        // AC1: appended rows carry a prev_hash linking to the predecessor;
        // the first row is None, the rest are Some.
        let temp = TempDir::new()?;
        let store = RocksDbStore::open(temp.path(), &test_config())?;
        append(&store, "u1", &ev("a", 100))?;
        append(&store, "u1", &ev("b", 200))?;
        append(&store, "u1", &ev("c", 300))?;

        let (blobs, dropped) = store.scan_audit("u1", usize::MAX);
        assert_eq!(dropped, 0);
        assert_eq!(blobs.len(), 3);
        let parsed: Vec<AuditEvent> = blobs
            .iter()
            .map(|b| serde_json::from_slice(b).expect("parse"))
            .collect();
        assert!(parsed[0].prev_hash.is_none(), "first event = chain root");
        assert!(parsed[1].prev_hash.is_some());
        assert!(parsed[2].prev_hash.is_some());
        // and the chain verifies end-to-end through storage
        assert_eq!(
            verify_chain(&store, "u1"),
            ChainStatus::Valid { chained: 2 }
        );
        Ok(())
    }

    #[test]
    fn test_last_audit_reverse_seek_respects_user_boundary() -> Result<()> {
        // Greptile #251: the O(1) reverse-seek `last_audit` must return the
        // newest row for THIS user, never bleed into a neighbouring prefix.
        // Interleave two users; if last_audit picked the wrong predecessor,
        // one of the chains would fail to verify.
        let temp = TempDir::new()?;
        let store = RocksDbStore::open(temp.path(), &test_config())?;
        append(&store, "u1", &ev("a1", 100))?;
        append(&store, "u2", &ev("b1", 110))?;
        append(&store, "u1", &ev("a2", 200))?;
        append(&store, "u2", &ev("b2", 210))?;
        append(&store, "u1", &ev("a3", 300))?;

        assert_eq!(
            verify_chain(&store, "u1"),
            ChainStatus::Valid { chained: 2 }
        );
        assert_eq!(
            verify_chain(&store, "u2"),
            ChainStatus::Valid { chained: 1 }
        );
        Ok(())
    }

    #[test]
    fn test_verify_chain_valid() {
        let blobs = chained_blobs(&["a", "b", "c"]);
        assert_eq!(
            verify_chain_blobs(&blobs),
            ChainStatus::Valid { chained: 2 }
        );
    }

    #[test]
    fn test_verify_chain_detects_edit() {
        // AC2: editing a row's content (keeping valid JSON) breaks the link of
        // the *next* row, whose prev_hash no longer matches.
        let mut blobs = chained_blobs(&["a", "b", "c"]);
        let mut edited: AuditEvent = serde_json::from_slice(&blobs[1]).unwrap();
        edited.action = "TAMPERED".into();
        blobs[1] = serde_json::to_vec(&edited).unwrap();
        match verify_chain_blobs(&blobs) {
            ChainStatus::Broken { index, .. } => assert_eq!(index, 2),
            other => panic!("expected Broken at index 2, got {other:?}"),
        }
    }

    #[test]
    fn test_verify_chain_detects_deletion() {
        // AC2: deleting a middle row leaves a prev_hash that points at the
        // removed predecessor → mismatch at the row after the gap.
        let mut blobs = chained_blobs(&["a", "b", "c", "d"]);
        blobs.remove(2); // drop "c"
        match verify_chain_blobs(&blobs) {
            ChainStatus::Broken { index, .. } => assert_eq!(index, 2),
            other => panic!("expected Broken at index 2, got {other:?}"),
        }
    }

    #[test]
    fn test_verify_chain_legacy_prefix_skipped() {
        // AC3: legacy rows (prev_hash=None, pre-/150d) before the first chained
        // row are skipped; the chain is verified from the first Some.
        let mut blobs: Vec<Vec<u8>> = Vec::new();
        // two legacy rows
        for a in ["old1", "old2"] {
            blobs.push(serde_json::to_vec(&ev(a, 1)).unwrap());
        }
        // chained continuation linking to the last legacy blob
        let mut prev = blob_hash(blobs.last().unwrap());
        for a in ["new1", "new2"] {
            let mut e = ev(a, 2);
            e.prev_hash = Some(prev.clone());
            let b = serde_json::to_vec(&e).unwrap();
            prev = blob_hash(&b);
            blobs.push(b);
        }
        assert_eq!(
            verify_chain_blobs(&blobs),
            ChainStatus::Valid { chained: 2 }
        );
    }

    #[test]
    fn test_verify_chain_none_after_started_is_break() {
        // A None prev_hash *after* the chain has started = a chained row was
        // replaced by an unlinked one → break.
        let mut blobs = chained_blobs(&["a", "b", "c"]);
        let mut unlinked: AuditEvent = serde_json::from_slice(&blobs[2]).unwrap();
        unlinked.prev_hash = None;
        blobs[2] = serde_json::to_vec(&unlinked).unwrap();
        match verify_chain_blobs(&blobs) {
            ChainStatus::Broken { index, .. } => assert_eq!(index, 2),
            other => panic!("expected Broken at index 2, got {other:?}"),
        }
    }

    #[test]
    fn test_verify_chain_detects_reorder() {
        // AC2: swapping two adjacent chained rows breaks the prev_hash links.
        let mut blobs = chained_blobs(&["a", "b", "c", "d"]);
        blobs.swap(1, 2);
        assert!(
            matches!(verify_chain_blobs(&blobs), ChainStatus::Broken { .. }),
            "reordering chained rows must be detected"
        );
    }
}
