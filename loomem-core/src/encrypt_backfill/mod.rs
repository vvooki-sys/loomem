//! Encryption-at-rest backfill service (cycle /147).
//!
//! Walks six row classes that may contain legacy plaintext written before
//! `LOOMEM_AT_REST_MASTER_KEY` was set and re-encrypts them through the
//! existing `store_chunk`/`store_entity`/`provider.encrypt`+`put` write paths.
//! Idempotent: rows that already satisfy the encrypted predicate are counted
//! `already_encrypted` and skipped with zero writes.
//!
//! See ADR-013 §7 for the full contract.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::backfill_trace::TraceLog;
use crate::crypto::at_rest::is_encrypted;
use crate::graph::{GraphStore, StoredEntityRead};
use crate::storage::keys::ENCRYPT_BACKFILL_PROGRESS_KEY;
use crate::storage::{RocksDbStore, StoredChunkRead};

// ── Public types ────────────────────────────────────────────────────────────

/// Caller-supplied parameters for a single backfill run.
pub struct EncryptBackfillParams {
    pub snapshot_token: String,
    pub batch_size: usize,
    pub inter_batch_sleep_ms: u64,
}

/// Per-class counters accumulated during a run.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct ClassCounters {
    pub scanned: u64,
    pub already_encrypted: u64,
    pub encrypted: u64,
    pub orphans: u64,
    /// Orphans attributed to a known scope (write/encrypt failures). Orphans
    /// whose scope is unresolvable (parse failures, missing paired chunk) are
    /// counted only in `orphans` — per ADR-013 §7 "per scope where the scope
    /// is knowable".
    #[serde(default)]
    pub orphans_by_scope: BTreeMap<String, u64>,
}

/// Persisted progress written to `ENCRYPT_BACKFILL_PROGRESS_KEY` after each
/// batch and on terminal events.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackfillProgress {
    /// `"running" | "completed" | "stopped_orphan_threshold" | "failed"`.
    pub status: String,
    pub snapshot_token: String,
    pub started_at: u64,
    pub updated_at: u64,
    pub per_class: BTreeMap<String, ClassCounters>,
    pub error: Option<String>,
}

/// Type alias to avoid repeating the boxed-slice pair in batch vectors.
type KvPair = (Box<[u8]>, Box<[u8]>);

// ── Soft/hard orphan thresholds (ADR-013 §7) ────────────────────────────────

const ORPHAN_WARN_PER_CLASS_SCOPE: u64 = 10;
const ORPHAN_STOP_PER_CLASS: u64 = 100;
const ORPHAN_STOP_PER_CLASS_SCOPE: u64 = 50;

// ── Main orchestrator ────────────────────────────────────────────────────────

/// Run the encrypt-at-rest backfill.
///
/// Returns `BackfillProgress` with a terminal status on completion or abort.
/// Returns `Err` only on pre-flight failures (NoopProvider or fatal I/O
/// errors). The handler wraps this in `tokio::spawn`.
pub async fn run_encrypt_backfill(
    store: &RocksDbStore,
    graph: &GraphStore,
    params: &EncryptBackfillParams,
    trace: &TraceLog,
) -> Result<BackfillProgress> {
    anyhow::ensure!(
        store.encryption_provider().is_enabled(),
        "encryption provider is disabled (NoopProvider); \
         backfill without a master key would produce plaintext rewrites"
    );

    let now_secs = unix_now();
    let mut progress = BackfillProgress {
        status: "running".to_string(),
        snapshot_token: params.snapshot_token.clone(),
        started_at: now_secs,
        updated_at: now_secs,
        per_class: BTreeMap::new(),
        error: None,
    };

    trace.emit(
        "encrypt_backfill_start",
        serde_json::json!({
            "token": params.snapshot_token,
            "batch_size": params.batch_size,
            "sleep_ms": params.inter_batch_sleep_ms,
        }),
    );
    save_progress(store, &progress);

    // Process chunk envelope classes (L0, L1, L2).
    for level in 0u8..=2 {
        let class = format!("chunk_L{level}");
        let prefix = format!("chunk:L{level}:");
        let stopped = process_chunk_class(store, params, &prefix, &class, &mut progress).await?;
        save_progress(store, &progress);
        trace.emit(
            "class_complete",
            serde_json::json!({ "class": class, "counters": progress.per_class.get(&class) }),
        );
        if stopped {
            return finish_stopped(store, &mut progress, trace);
        }
    }

    // Process graph:entity envelope class.
    {
        let class = "graph_entity";
        let stopped = process_entity_class(store, graph, params, class, &mut progress).await?;
        save_progress(store, &progress);
        trace.emit(
            "class_complete",
            serde_json::json!({ "class": class, "counters": progress.per_class.get(class) }),
        );
        if stopped {
            return finish_stopped(store, &mut progress, trace);
        }
    }

    // Raw classes: entity:, rel:, audit:, access:.
    for (class, prefix, scope_src) in raw_class_specs() {
        let stopped =
            process_raw_class(store, params, prefix, class, scope_src, &mut progress).await?;
        save_progress(store, &progress);
        trace.emit(
            "class_complete",
            serde_json::json!({ "class": class, "counters": progress.per_class.get(class) }),
        );
        if stopped {
            return finish_stopped(store, &mut progress, trace);
        }
    }

    progress.status = "completed".to_string();
    progress.updated_at = unix_now();
    save_progress(store, &progress);
    trace.emit(
        "encrypt_backfill_complete",
        serde_json::json!({ "report": &progress }),
    );
    Ok(progress)
}

// ── Chunk-envelope walker ────────────────────────────────────────────────────

async fn process_chunk_class(
    store: &RocksDbStore,
    params: &EncryptBackfillParams,
    prefix: &str,
    class: &str,
    progress: &mut BackfillProgress,
) -> Result<bool> {
    let counters = progress.per_class.entry(class.to_string()).or_default();
    let mut batch: Vec<KvPair> = Vec::with_capacity(params.batch_size);

    for (key, val) in store.prefix_scan(prefix.as_bytes()) {
        batch.push((key, val));
        if batch.len() >= params.batch_size {
            let stopped =
                flush_chunk_batch(store, std::mem::take(&mut batch), class, counters).await?;
            if stopped {
                return Ok(true);
            }
            sleep_if_needed(params).await;
        }
    }
    if !batch.is_empty() {
        let stopped = flush_chunk_batch(store, batch, class, counters).await?;
        if stopped {
            return Ok(true);
        }
    }
    Ok(orphan_limits_hit(counters))
}

async fn flush_chunk_batch(
    store: &RocksDbStore,
    batch: Vec<KvPair>,
    class: &str,
    counters: &mut ClassCounters,
) -> Result<bool> {
    for (_key, val) in batch {
        counters.scanned += 1;
        let staged: StoredChunkRead = match serde_json::from_slice(&val) {
            Ok(v) => v,
            Err(e) => {
                warn!(class, error = %e, "chunk envelope parse failed — skipping (orphan)");
                if record_orphan(class, None, counters) {
                    return Ok(true);
                }
                continue;
            }
        };
        if !staged.encrypted_payload.is_empty() {
            counters.already_encrypted += 1;
            continue;
        }
        // Re-encrypt through the existing store_chunk write path.
        if let Err(e) = store.store_chunk(&staged.chunk) {
            warn!(class, chunk_id = %staged.chunk.id, error = %e, "store_chunk failed — orphan");
            if record_orphan(class, Some(&staged.chunk.stream), counters) {
                return Ok(true);
            }
            continue;
        }
        counters.encrypted += 1;
    }
    Ok(false)
}

// ── Graph-entity envelope walker ─────────────────────────────────────────────

async fn process_entity_class(
    store: &RocksDbStore,
    graph: &GraphStore,
    params: &EncryptBackfillParams,
    class: &str,
    progress: &mut BackfillProgress,
) -> Result<bool> {
    let counters = progress.per_class.entry(class.to_string()).or_default();
    let prefix = b"graph:entity:" as &[u8];
    let mut batch: Vec<KvPair> = Vec::with_capacity(params.batch_size);

    for (key, val) in store.prefix_scan(prefix) {
        batch.push((key, val));
        if batch.len() >= params.batch_size {
            let stopped =
                flush_entity_batch(graph, std::mem::take(&mut batch), class, counters).await?;
            if stopped {
                return Ok(true);
            }
            sleep_if_needed(params).await;
        }
    }
    if !batch.is_empty() {
        let stopped = flush_entity_batch(graph, batch, class, counters).await?;
        if stopped {
            return Ok(true);
        }
    }
    Ok(orphan_limits_hit(counters))
}

async fn flush_entity_batch(
    graph: &GraphStore,
    batch: Vec<KvPair>,
    class: &str,
    counters: &mut ClassCounters,
) -> Result<bool> {
    for (_key, val) in batch {
        counters.scanned += 1;
        let staged: StoredEntityRead = match serde_json::from_slice(&val) {
            Ok(v) => v,
            Err(e) => {
                warn!(class, error = %e, "entity envelope parse failed — orphan");
                if record_orphan(class, None, counters) {
                    return Ok(true);
                }
                continue;
            }
        };
        if !staged.encrypted_payload.is_empty() {
            counters.already_encrypted += 1;
            continue;
        }
        // Orphan guard: no stream_id means we can't resolve the DEK scope.
        if staged.entity.stream_id.is_empty() {
            warn!(class, entity_id = %staged.entity.id, "empty stream_id — orphan");
            if record_orphan(class, None, counters) {
                return Ok(true);
            }
            continue;
        }
        if let Err(e) = graph.store_entity(&staged.entity) {
            warn!(class, entity_id = %staged.entity.id, error = %e, "store_entity failed — orphan");
            if record_orphan(class, Some(&staged.entity.stream_id), counters) {
                return Ok(true);
            }
            continue;
        }
        counters.encrypted += 1;
    }
    Ok(false)
}

// ── Raw-blob walker (entity:, rel:, audit:, access:) ─────────────────────────

/// How to derive the encryption scope from a raw key.
#[derive(Clone, Copy)]
pub(crate) enum ScopeSource {
    /// Scope = `chunk.stream` from the paired `chunk:<chunk_id>` row.
    PairedChunk,
    /// Scope = segment at `key.rsplitn(3, ':').nth(2)` (for audit/access,
    /// where ts and seq are fixed-width suffixes).
    KeyPrefix,
}

fn raw_class_specs() -> [(&'static str, &'static str, ScopeSource); 4] {
    [
        ("entity", "entity:", ScopeSource::PairedChunk),
        ("rel", "rel:", ScopeSource::PairedChunk),
        ("audit", "audit:", ScopeSource::KeyPrefix),
        ("access", "access:", ScopeSource::KeyPrefix),
    ]
}

async fn process_raw_class(
    store: &RocksDbStore,
    params: &EncryptBackfillParams,
    prefix: &str,
    class: &str,
    scope_src: ScopeSource,
    progress: &mut BackfillProgress,
) -> Result<bool> {
    let counters = progress.per_class.entry(class.to_string()).or_default();
    let mut batch: Vec<KvPair> = Vec::with_capacity(params.batch_size);

    for (key, val) in store.prefix_scan(prefix.as_bytes()) {
        batch.push((key, val));
        if batch.len() >= params.batch_size {
            let stopped = flush_raw_batch(
                store,
                std::mem::take(&mut batch),
                class,
                scope_src,
                counters,
            )
            .await?;
            if stopped {
                return Ok(true);
            }
            sleep_if_needed(params).await;
        }
    }
    if !batch.is_empty() {
        let stopped = flush_raw_batch(store, batch, class, scope_src, counters).await?;
        if stopped {
            return Ok(true);
        }
    }
    Ok(orphan_limits_hit(counters))
}

async fn flush_raw_batch(
    store: &RocksDbStore,
    batch: Vec<KvPair>,
    class: &str,
    scope_src: ScopeSource,
    counters: &mut ClassCounters,
) -> Result<bool> {
    for (key, val) in batch {
        counters.scanned += 1;
        if is_encrypted(&val) {
            counters.already_encrypted += 1;
            continue;
        }
        let key_str = String::from_utf8_lossy(&key);
        let scope = match resolve_scope(store, &key_str, class, scope_src) {
            Ok(s) => s,
            Err(_) => {
                warn!(class, key = %key_str, "scope resolution failed — orphan");
                if record_orphan(class, None, counters) {
                    return Ok(true);
                }
                continue;
            }
        };
        let encrypted = match store
            .encryption_provider()
            .encrypt(&scope, &val)
            .context("encrypt raw blob")
        {
            Ok(v) => v,
            Err(e) => {
                warn!(class, key = %key_str, error = %e, "encrypt failed — orphan");
                if record_orphan(class, Some(&scope), counters) {
                    return Ok(true);
                }
                continue;
            }
        };
        // Write failure = orphan (skip+log+count, D5) — mirrors the encrypt arm
        // and the chunk/entity walkers; the thresholds decide whether to abort
        // (Greptile #266 P1: a transient single-row write stall must not kill
        // the whole run).
        if let Err(e) = store.put(&key, &encrypted) {
            warn!(class, key = %key_str, error = %e, "put failed — orphan");
            if record_orphan(class, Some(&scope), counters) {
                return Ok(true);
            }
            continue;
        }
        counters.encrypted += 1;
    }
    Ok(false)
}

fn resolve_scope(
    store: &RocksDbStore,
    key_str: &str,
    class: &str,
    scope_src: ScopeSource,
) -> Result<String> {
    match scope_src {
        ScopeSource::PairedChunk => paired_chunk_scope(store, key_str, class),
        ScopeSource::KeyPrefix => key_prefix_scope(key_str, class),
    }
}

/// Scope for `entity:{chunk_id}` / `rel:{chunk_id}` rows: the paired chunk's
/// `stream`. A missing paired chunk is the canonical orphan case (D5).
fn paired_chunk_scope(store: &RocksDbStore, key_str: &str, class: &str) -> Result<String> {
    let chunk_id = key_str
        .split_once(':')
        .map(|x| x.1)
        .with_context(|| format!("malformed {class} key: {key_str}"))?;
    let chunk = store
        .get_chunk(chunk_id)
        .with_context(|| format!("get_chunk for {class} scope"))?
        .with_context(|| format!("paired chunk missing for {class} key {key_str}"))?;
    Ok(chunk.stream)
}

/// Scope embedded in the key for `audit:{user_id}:{ts:020}:{seq:06}` and
/// `access:{stream}:{ts:020}:{seq:06}` rows. ts and seq are fixed-width
/// suffixes; split from the right so the middle segment (user_id/stream) may
/// contain arbitrary characters.
fn key_prefix_scope(key_str: &str, class: &str) -> Result<String> {
    let mut parts = key_str.rsplitn(3, ':');
    let _seq = parts.next(); // seq:06
    let _ts = parts.next(); // ts:020
    let remainder = parts
        .next()
        .with_context(|| format!("malformed {class} key (too few segments): {key_str}"))?;
    // remainder = `audit:{user_id}` or `access:{stream}`.
    let scope = remainder
        .split_once(':')
        .map(|x| x.1)
        .with_context(|| format!("malformed {class} key (no colon in prefix): {key_str}"))?;
    anyhow::ensure!(!scope.is_empty(), "empty scope in {class} key {key_str}");
    Ok(scope.to_string())
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn save_progress(store: &RocksDbStore, progress: &BackfillProgress) {
    match serde_json::to_vec(progress) {
        Ok(bytes) => {
            if let Err(e) = store.put(ENCRYPT_BACKFILL_PROGRESS_KEY, &bytes) {
                warn!(error = %e, "failed to persist backfill progress");
            }
        }
        Err(e) => warn!(error = %e, "failed to serialize backfill progress"),
    }
}

/// Record one orphan and decide whether the run must stop (ADR-013 §7
/// thresholds: soft WARN ≥10 per class·scope, hard STOP ≥100 per class OR
/// ≥50 per class·scope). `scope = None` for orphans whose scope is
/// unresolvable — those count only toward the per-class total.
fn record_orphan(class: &str, scope: Option<&str>, counters: &mut ClassCounters) -> bool {
    counters.orphans += 1;
    if let Some(scope) = scope {
        let n = counters
            .orphans_by_scope
            .entry(scope.to_string())
            .or_default();
        *n += 1;
        if *n == ORPHAN_WARN_PER_CLASS_SCOPE {
            warn!(class, scope, orphans = *n, "soft orphan threshold reached");
        }
        if *n >= ORPHAN_STOP_PER_CLASS_SCOPE {
            warn!(
                class,
                scope,
                orphans = *n,
                "per-scope hard orphan threshold reached — stopping backfill"
            );
            return true;
        }
    } else if counters.orphans < ORPHAN_STOP_PER_CLASS
        && counters.orphans.is_multiple_of(ORPHAN_WARN_PER_CLASS_SCOPE)
    {
        // Below-hard-stop guard: at the 100 boundary only the hard-stop warn
        // fires (Greptile #266 P2 — no double-warning for the same event).
        warn!(
            class,
            orphans = counters.orphans,
            "soft orphan threshold reached (scope unknown)"
        );
    }
    if counters.orphans >= ORPHAN_STOP_PER_CLASS {
        warn!(
            class,
            orphans = counters.orphans,
            "hard orphan threshold reached — stopping backfill"
        );
        return true;
    }
    false
}

/// Pure threshold predicate for end-of-class checks (no recording).
fn orphan_limits_hit(counters: &ClassCounters) -> bool {
    counters.orphans >= ORPHAN_STOP_PER_CLASS
        || counters
            .orphans_by_scope
            .values()
            .any(|n| *n >= ORPHAN_STOP_PER_CLASS_SCOPE)
}

async fn sleep_if_needed(params: &EncryptBackfillParams) {
    if params.inter_batch_sleep_ms > 0 {
        tokio::time::sleep(std::time::Duration::from_millis(
            params.inter_batch_sleep_ms,
        ))
        .await;
    }
}

fn finish_stopped(
    store: &RocksDbStore,
    progress: &mut BackfillProgress,
    trace: &TraceLog,
) -> Result<BackfillProgress> {
    progress.status = "stopped_orphan_threshold".to_string();
    progress.updated_at = unix_now();
    save_progress(store, progress);
    trace.emit(
        "orphan_stop",
        serde_json::json!({ "counts": &progress.per_class }),
    );
    Ok(progress.clone())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests;
