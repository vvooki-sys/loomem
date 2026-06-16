//! Graph-entity stream-repair service (cycle /147a).
//!
//! Walks `graph:entity:*` rows that have an empty `stream_id` (legacy cohort
//! written before multi-stream support) and resolves the stream from the
//! entity's paired chunks. Repairs are written through `graph.store_entity`
//! (the existing encrypted chokepoint) and the stream-scoped name/alias
//! indexes are written inline, mirroring `get_or_create_entity`.
//!
//! `dry_run = true` (the default) classifies every row and produces counts
//! with zero writes. `dry_run = false` performs the writes.

use std::collections::{BTreeMap, HashSet};

use anyhow::{Context, Result};
use serde::Serialize;
use tracing::warn;

use crate::graph::{EntityNode, GraphStore, StoredEntityRead};
use crate::storage::RocksDbStore;

// ── Public report type ───────────────────────────────────────────────────────

/// Counters returned by [`repair_entity_streams`].
#[derive(Debug, Default, Serialize)]
pub struct RepairReport {
    pub dry_run: bool,
    pub scanned: u64,
    /// Already has a non-empty `stream_id` OR `encrypted_payload` non-empty.
    pub already_scoped: u64,
    pub repaired: u64,
    /// Repaired but index rows skipped — incumbent entity exists in stream.
    pub repaired_name_conflict: u64,
    /// Chunk set has members from >1 distinct stream.
    pub conflicting_chunk_streams: u64,
    /// No live chunks (or all chunks missing from store).
    pub unresolvable_no_chunks: u64,
    pub malformed: u64,
    /// Alias index rows skipped because the alias token already resolves to a
    /// DIFFERENT entity in the target stream (never overwrite an incumbent's
    /// mapping; the repaired entity stays reachable by name/id, just not by
    /// that one colliding alias).
    pub alias_collisions_skipped: u64,
    /// Repair counts per stream id (only populated for `dry_run = false`).
    pub repaired_by_stream: BTreeMap<String, u64>,
}

// ── Main entry point ─────────────────────────────────────────────────────────

/// Scan every `graph:entity:*` row and repair those with an empty `stream_id`.
///
/// Requires an active encryption provider (`store.encryption_provider().is_enabled()`).
/// Returns `Err` when the provider is disabled — callers should check this
/// before calling and return HTTP 400.
pub fn repair_entity_streams(
    store: &RocksDbStore,
    graph: &GraphStore,
    dry_run: bool,
) -> Result<RepairReport> {
    anyhow::ensure!(
        store.encryption_provider().is_enabled(),
        "encryption provider is disabled (NoopProvider); \
         repair writes encrypted rows and requires LOOMEM_AT_REST_MASTER_KEY"
    );

    let mut report = RepairReport {
        dry_run,
        ..Default::default()
    };

    let prefix = b"graph:entity:" as &[u8];
    // Collect into a Vec so we don't hold the iterator (and thus a DB read lock)
    // across the write operations that follow.
    type KvPair = (Box<[u8]>, Box<[u8]>);
    let rows: Vec<KvPair> = store.prefix_scan(prefix).collect();

    for (_key, val) in rows {
        report.scanned += 1;
        process_row(store, graph, &val, dry_run, &mut report)?;
    }

    Ok(report)
}

// ── Row processor ────────────────────────────────────────────────────────────

fn process_row(
    store: &RocksDbStore,
    graph: &GraphStore,
    val: &[u8],
    dry_run: bool,
    report: &mut RepairReport,
) -> Result<()> {
    // Parse-fail → malformed, skip.
    let staged: StoredEntityRead = match serde_json::from_slice(val) {
        Ok(v) => v,
        Err(e) => {
            warn!(error = %e, "graph_repair: entity parse failed — malformed, skipping");
            report.malformed += 1;
            return Ok(());
        }
    };

    // Already encrypted OR already has a stream → nothing to do.
    if !staged.encrypted_payload.is_empty() || !staged.entity.stream_id.is_empty() {
        report.already_scoped += 1;
        return Ok(());
    }

    // Resolve stream from paired chunks.
    match resolve_streams_for_entity(store, &staged.entity)? {
        StreamResolution::NoChunks => {
            report.unresolvable_no_chunks += 1;
        }
        StreamResolution::Conflicting => {
            report.conflicting_chunk_streams += 1;
        }
        StreamResolution::Single(stream) => {
            apply_repair(store, graph, staged.entity, stream, dry_run, report)?;
        }
    }

    Ok(())
}

// ── Stream resolution ────────────────────────────────────────────────────────

enum StreamResolution {
    Single(String),
    Conflicting,
    NoChunks,
}

fn resolve_streams_for_entity(
    store: &RocksDbStore,
    entity: &EntityNode,
) -> Result<StreamResolution> {
    let mut streams: HashSet<String> = HashSet::new();

    for chunk_id in &entity.chunk_ids {
        if let Some(chunk) = store
            .get_chunk(chunk_id)
            .with_context(|| format!("get_chunk {chunk_id}"))?
        {
            // Tombstoned chunks do not vote: a soft-deleted chunk's stream is an
            // association the user removed — binding an entity to it would
            // resurrect it (critic MED-1; filter-parity with list_active_streams).
            if chunk.deleted_at.is_none() && !chunk.stream.is_empty() {
                streams.insert(chunk.stream);
            }
        }
    }

    let mut it = streams.into_iter();
    match (it.next(), it.next()) {
        (None, _) => Ok(StreamResolution::NoChunks),
        (Some(stream), None) => Ok(StreamResolution::Single(stream)),
        (Some(_), Some(_)) => Ok(StreamResolution::Conflicting),
    }
}

// ── Repair application ───────────────────────────────────────────────────────

fn apply_repair(
    store: &RocksDbStore,
    graph: &GraphStore,
    mut entity: EntityNode,
    stream: String,
    dry_run: bool,
    report: &mut RepairReport,
) -> Result<()> {
    // Check for an incumbent with the same canonical name in stream S.
    let incumbent = graph
        .get_entity_by_name(&entity.canonical_name, &stream)
        .with_context(|| {
            format!(
                "get_entity_by_name '{}' in stream '{stream}'",
                entity.canonical_name
            )
        })?;

    let has_conflict = incumbent
        .as_ref()
        .map(|e| e.id != entity.id)
        .unwrap_or(false);

    if dry_run {
        if has_conflict {
            report.repaired_name_conflict += 1;
        } else {
            report.repaired += 1;
        }
        return Ok(());
    }

    // Mutate only stream_id; no timestamp bumps per contract.
    entity.stream_id = stream.clone();

    // Write the entity row (encrypted via chokepoint).
    graph
        .store_entity(&entity)
        .with_context(|| format!("store_entity id={}", entity.id))?;

    if has_conflict {
        // Incumbent exists under the same name — do NOT write index rows.
        report.repaired_name_conflict += 1;
    } else {
        // Write stream-scoped name and alias index rows.
        // mirrors get_or_create_entity index writes (graph/mod.rs:255) — no shared helper exists
        let skipped = write_index_rows(store, graph, &entity, &stream)?;
        report.alias_collisions_skipped += skipped;
        report.repaired += 1;
        *report.repaired_by_stream.entry(stream).or_default() += 1;
    }

    Ok(())
}

// ── Index row writer ─────────────────────────────────────────────────────────

/// Write name and alias index entries under stream `stream` for a repaired entity.
/// Returns the number of alias rows SKIPPED due to collision with a different
/// incumbent's mapping (critic MED-2: never overwrite an existing alias→id row).
///
/// Mirrors the index-write block in `get_or_create_entity` / `create_new_entity_with_indexes`
/// (graph/mod.rs ~255–265, ~960–969). No shared helper exists between graph/mod.rs and
/// this module — the comment is kept as an in-code navigation aid.
fn write_index_rows(
    store: &RocksDbStore,
    graph: &GraphStore,
    entity: &EntityNode,
    stream: &str,
) -> Result<u64> {
    let provider = store.encryption_provider();
    let sp = graph.stream_prefix(stream);
    let mut skipped: u64 = 0;

    let name_token = provider
        .index_token(stream, &entity.canonical_name)
        .with_context(|| format!("index_token for canonical_name '{}'", entity.canonical_name))?;

    // Name index. Safe without a per-row check: the caller verified no incumbent
    // resolves under this canonical name (has_conflict == false covers both the
    // name: and alias: key for `name_token` via get_entity_by_name).
    put_index_entry(store, &format!("{}name:{}", sp, name_token), &entity.id)?;

    // Per-alias index entries (mirrors graph/mod.rs:963-968), collision-checked.
    for alias in &entity.aliases {
        skipped += write_alias_row(store, graph, entity, stream, &sp, alias)?;
    }

    // Canonical name also indexed as alias (mirrors graph/mod.rs:969). Covered
    // by the caller's has_conflict check (same token as the name: row).
    put_index_entry(store, &format!("{}alias:{}", sp, name_token), &entity.id)?;

    Ok(skipped)
}

/// Write one alias index row unless the alias already resolves to a DIFFERENT
/// entity in this stream — never redirect an incumbent's alias (critic MED-2).
/// Returns 1 when the row was skipped due to collision, 0 when written.
fn write_alias_row(
    store: &RocksDbStore,
    graph: &GraphStore,
    entity: &EntityNode,
    stream: &str,
    sp: &str,
    alias: &str,
) -> Result<u64> {
    match graph
        .get_entity_by_name(alias, stream)
        .with_context(|| format!("alias collision probe '{alias}'"))?
    {
        Some(existing) if existing.id != entity.id => {
            warn!(
                alias,
                stream,
                incumbent_id = %existing.id,
                repaired_id = %entity.id,
                "graph_repair: alias already mapped to another entity — skipping"
            );
            Ok(1)
        }
        _ => {
            let alias_token = store
                .encryption_provider()
                .index_token(stream, alias)
                .with_context(|| format!("index_token for alias '{alias}'"))?;
            put_index_entry(store, &format!("{sp}alias:{alias_token}"), &entity.id)?;
            Ok(0)
        }
    }
}

/// Write a single `key → entity_id` byte entry to the store.
fn put_index_entry(store: &RocksDbStore, key: &str, entity_id: &str) -> Result<()> {
    store
        .put(key.as_bytes(), entity_id.as_bytes())
        .with_context(|| format!("put index entry {key}"))
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests;
