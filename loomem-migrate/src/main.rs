//! loomem-migrate — one-time migration utilities.
//!
//! Subcommands:
//!   --migrate-graph-entity-streams         Cycle /33b: re-stamp graph entities + edges
//!                                          whose stream_id is out of sync with their
//!                                          chunks' stream (C1 leftover). Dry-run by
//!                                          default, --commit applies.
//!   --validate-graph-entity-streams        Cycle /33b: read-only audit. Non-zero exit
//!                                          when mis-aligned count > 0.
//!   --sample-embeddings                    Cycle A1: read-only embedding sampler for
//!                                          the anisotropy diagnostic.

use anyhow::{Context, Result};
use rocksdb::{IteratorMode, Options, DB};
use serde_json::Value;
use std::path::{Path, PathBuf};

mod sample_embeddings;

/// Canonical source: `loomem_core::storage::keys::TANTIVY_REBUILD_FLAG_KEY`.
/// Mirror kept here — migrate is intentionally self-contained (no loomem-core
/// dep) so it can run against offline snapshots. Cycle /50 extracted the
/// canonical const; this local mirror tracks it. If the key changes, update
/// both locations in lockstep.
const TANTIVY_REBUILD_FLAG_KEY: &[u8] = b"meta:tantivy_rebuild_needed";

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();

    if args.iter().any(|a| a == "--migrate-graph-entity-streams") {
        return cmd_migrate_graph_entity_streams(&args);
    }

    if args.iter().any(|a| a == "--validate-graph-entity-streams") {
        return cmd_validate_graph_entity_streams(&args);
    }

    // Cycle A1: read-only embedding sampler for anisotropy diagnostic.
    // Accept both flag-style (`--sample-embeddings`) and subcommand-style
    // (`sample-embeddings`) per operator stop-and-ask response.
    if args
        .iter()
        .any(|a| a == "--sample-embeddings" || a == "sample-embeddings")
    {
        let db_path = resolve_db_path(&args);
        return sample_embeddings::cmd_sample_embeddings(&args, &db_path);
    }

    // No subcommand matched — print usage hint
    eprintln!("loomem-migrate: no subcommand specified. Use --help or pass a migration flag.");
    eprintln!("  Available: --migrate-graph-entity-streams, --validate-graph-entity-streams,");
    eprintln!("             --sample-embeddings");
    std::process::exit(1);
}

fn extract_flag_value(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn now_ts_readable() -> String {
    let (secs, nanos) = match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => (d.as_secs(), d.subsec_nanos()),
        Err(_) => (0, 0),
    };
    // Readable UTC stamp without pulling chrono into this crate.
    // Format: YYYY-MM-DD-HHMMSS-nnn — seconds + milliseconds so two
    // back-to-back invocations (tests, idempotent reruns) don't collide on
    // the backup directory name.
    let (y, m, d, hh, mm) = unix_to_utc_parts(secs);
    let ss = (secs % 60) as u32;
    let ms = nanos / 1_000_000;
    format!("{y:04}-{m:02}-{d:02}-{hh:02}{mm:02}{ss:02}-{ms:03}")
}

/// Convert unix seconds → (year, month, day, hour, minute) in UTC. Naive
/// Gregorian arithmetic — accurate for dates ≥ 2000 which is all we need
/// for backup file names. Avoids pulling in chrono just for a timestamp.
#[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
fn unix_to_utc_parts(secs: u64) -> (i64, u32, u32, u32, u32) {
    let days = (secs / 86_400) as i64;
    let seconds_of_day = (secs % 86_400) as u32;
    let hh = seconds_of_day / 3600;
    let mm = (seconds_of_day % 3600) / 60;

    // Days since 1970-01-01 → civil date (Howard Hinnant algorithm, civil_from_days).
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d, hh, mm)
}

/// Create a RocksDB checkpoint snapshot for backup before mutating the DB.
fn checkpoint_backup(db: &DB, dest: &Path) -> Result<()> {
    let cp = rocksdb::checkpoint::Checkpoint::new(db).context("new Checkpoint")?;
    cp.create_checkpoint(dest)
        .with_context(|| format!("create checkpoint at {}", dest.display()))
}

fn now_unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Where should the manifest file be written? Defaults to the current
/// directory. Overridable via `--manifest-dir <path>` — operator may point
/// this at a dedicated secrets directory; tests use it for isolation so
/// parallel runs do not fight over cwd.
fn manifest_dir_from_args(args: &[String]) -> PathBuf {
    extract_flag_value(args, "--manifest-dir")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

#[cfg(unix)]
fn write_owner_only(path: &Path, body: &[u8]) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("open {} for manifest write", path.display()))?;
    f.write_all(body)
        .with_context(|| format!("write manifest to {}", path.display()))?;
    Ok(())
}

#[cfg(not(unix))]
fn write_owner_only(path: &Path, body: &[u8]) -> Result<()> {
    std::fs::write(path, body).with_context(|| format!("write manifest to {}", path.display()))
}

// ── cycle/33b — graph entity + edge stream_id migration ──────────────────────
//
// Closes C1 leftover: shared stream migration (cycle C1) rewrote chunks from
// pre-C1 user streams to the shared stream, but left graph entities + edges
// on their old stream_id. Symptom: Graph/Private shows the old entity
// cluster, Graph/Shared shows only recent post-C1 probes.
//
// Storage model (from loomem-core/src/graph.rs, confirmed pre-impl):
//   Entity data : `graph:entity:{id}`           (global by id, stream in value)
//   Edge data   : `graph:edge:{id}`             (global by id, stream in value)
//   Name index  : `graph:s:{stream_id}:name:{canonical_lower}`   → entity_id
//   Alias index : `graph:s:{stream_id}:alias:{alias_lower}`      → entity_id
//   Chunk rev   : `graph:chunk:{chunk_id}`      (not stream-scoped)
//   Adjacency   : `graph:adj:{source}:{edge_id}` / `graph:radj:...` (entity-id
//                 scoped, NOT stream-scoped — so no index keys to move for
//                 edges; only the `stream_id` field inside edge JSON value).
//
// Migration rules — per entity:
//   1. Read entity; inspect entity.chunk_ids[0]. Look up chunk → compare
//      chunk.stream vs entity.stream_id.
//   2. Category: already_correct / mis_aligned / orphan / mixed_streams.
//   3. For mis_aligned — on commit, mutate entity.stream_id to chunk[0]'s
//      stream, re-serialize under SAME key `graph:entity:{id}`, move the
//      per-stream name + alias indices from the old stream_prefix to the
//      new one. Chunk reverse index `graph:chunk:{chunk_id}` is not stream-
//      scoped so it's untouched.
//
// Migration rules — per edge:
//   1. Read edge; look up source + target entity post-migration stream_ids.
//   2. If source and target are in the same stream AND it differs from
//      edge.stream_id — re-stamp the edge (single-value mutation, no index
//      keys to move).
//   3. If source and target differ (cross-stream) — skip + flag. Heuristics
//      are explicitly forbidden (brief §6 risk #2, AC-1.5).

use std::path::PathBuf as PathBuf33b;

/// Per-category counts for entities (also reused for edges where relevant).
#[derive(Default, Debug)]
struct GraphMigrationStats {
    entities_scanned: u64,
    entities_already_correct: u64,
    entities_mis_aligned: u64,
    entities_orphan: u64,
    entities_mixed_streams: u64,

    edges_scanned: u64,
    edges_already_correct: u64,
    edges_mis_aligned: u64,
    edges_orphan_entity: u64,
    edges_cross_stream: u64,
}

/// Sample record surfaced in the manifest (first N per category).
#[derive(Debug)]
struct EntityRewriteSample {
    id: String,
    canonical_name: String,
    old_stream: String,
    new_stream: String,
}

#[derive(Debug)]
struct EntitySkipSample {
    id: String,
    canonical_name: String,
    stream_id: String,
    /// For mixed-streams entities: list of distinct chunk streams.
    chunk_streams: Vec<String>,
}

/// Locate a chunk by id, scanning all three levels (matches loomem-core's
/// `RocksDbStore::get_chunk` behavior). Returns the chunk's `stream` field.
fn chunk_stream_by_id(db: &DB, chunk_id: &str) -> Result<Option<String>> {
    for level in 0..=2 {
        let key = format!("chunk:L{level}:{chunk_id}");
        if let Some(bytes) = db
            .get(key.as_bytes())
            .with_context(|| format!("read chunk {key}"))?
        {
            let v: Value = match serde_json::from_slice(&bytes) {
                Ok(v) => v,
                Err(_) => continue,
            };
            return Ok(v.get("stream").and_then(Value::as_str).map(str::to_string));
        }
    }
    Ok(None)
}

/// Category of an entity after comparing its `stream_id` to its chunks' streams.
enum EntityCategory {
    AlreadyCorrect,
    /// Needs rewrite. Carries the target stream (chunk[0]'s stream).
    MisAligned {
        new_stream: String,
    },
    /// chunk_ids empty or all chunks missing — out of scope, skip.
    Orphan,
    /// chunks exist in >1 distinct streams — manual review, skip + flag.
    MixedStreams {
        chunk_streams: Vec<String>,
    },
}

fn classify_entity(db: &DB, entity: &Value) -> Result<EntityCategory> {
    let current_stream = entity
        .get("stream_id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let chunk_ids: Vec<String> = entity
        .get("chunk_ids")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();

    if chunk_ids.is_empty() {
        return Ok(EntityCategory::Orphan);
    }

    // Collect chunk streams; a chunk missing from storage is treated as a
    // soft orphan — skip without crashing.
    let mut streams: Vec<String> = Vec::new();
    for cid in &chunk_ids {
        if let Some(s) = chunk_stream_by_id(db, cid)? {
            if !streams.contains(&s) {
                streams.push(s);
            }
        }
    }

    if streams.is_empty() {
        return Ok(EntityCategory::Orphan);
    }
    if streams.len() > 1 {
        return Ok(EntityCategory::MixedStreams {
            chunk_streams: streams,
        });
    }
    let only = streams.into_iter().next().unwrap_or_default();
    if only == current_stream {
        Ok(EntityCategory::AlreadyCorrect)
    } else {
        Ok(EntityCategory::MisAligned { new_stream: only })
    }
}

/// Stream-prefix for per-stream name + alias indices (matches
/// `GraphStore::stream_prefix`). Kept in sync here to avoid an loomem-core
/// dependency; if graph.rs ever changes this format this const must move
/// too — guarded by an integration test seeding via GraphStore would catch it.
fn graph_stream_prefix_33b(stream_id: &str) -> String {
    format!("graph:s:{stream_id}:")
}

/// Collect all per-stream index keys that must move when an entity migrates
/// from `old_stream` to `new_stream`. Returns `(old_key, new_key, value)`
/// tuples. Includes canonical-name index + all alias indices + the
/// canonical-name-as-alias entry (mirrors `get_or_create_entity:137-140`).
fn entity_index_moves(
    entity: &Value,
    old_stream: &str,
    new_stream: &str,
) -> Vec<(String, String, String)> {
    let id = entity.get("id").and_then(Value::as_str).unwrap_or("");
    let canonical = entity
        .get("canonical_name")
        .and_then(Value::as_str)
        .unwrap_or("");
    let aliases: Vec<String> = entity
        .get("aliases")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();

    let old_sp = graph_stream_prefix_33b(old_stream);
    let new_sp = graph_stream_prefix_33b(new_stream);
    let mut out: Vec<(String, String, String)> = Vec::new();

    if !canonical.is_empty() {
        let canon_lower = canonical.to_lowercase();
        out.push((
            format!("{old_sp}name:{canon_lower}"),
            format!("{new_sp}name:{canon_lower}"),
            id.to_string(),
        ));
        // Canonical name is also indexed as an alias (see graph.rs:137-140).
        out.push((
            format!("{old_sp}alias:{canon_lower}"),
            format!("{new_sp}alias:{canon_lower}"),
            id.to_string(),
        ));
    }
    for alias in &aliases {
        let lower = alias.to_lowercase();
        out.push((
            format!("{old_sp}alias:{lower}"),
            format!("{new_sp}alias:{lower}"),
            id.to_string(),
        ));
    }
    out
}

/// Commit an entity rewrite: mutate `entity.stream_id` + `updated_at`, put
/// value back under the same global key, move per-stream indices. Caller is
/// responsible for passing a pre-categorised `MisAligned` entity.
fn rewrite_entity(db: &DB, entity_json: &mut Value, key: &[u8], new_stream: &str) -> Result<()> {
    let old_stream = entity_json
        .get("stream_id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    // Collect index moves BEFORE mutating the value so we can read the
    // canonical_name + aliases off the original record.
    let moves = entity_index_moves(entity_json, &old_stream, new_stream);

    if let Some(obj) = entity_json.as_object_mut() {
        obj.insert(
            "stream_id".to_string(),
            Value::String(new_stream.to_string()),
        );
        obj.insert(
            "updated_at".to_string(),
            Value::Number(serde_json::Number::from(now_unix_secs())),
        );
    }
    let bytes = serde_json::to_vec(entity_json).context("serialize rewritten entity")?;
    db.put(key, &bytes).context("put rewritten entity")?;

    for (old_key, new_key, value) in moves {
        // Order: put new then delete old so a mid-flight crash leaves a
        // readable name-index pointing at the post-migration stream. Put
        // itself is idempotent if run twice (same value).
        db.put(new_key.as_bytes(), value.as_bytes())
            .with_context(|| format!("put new index {new_key}"))?;
        if new_key != old_key {
            db.delete(old_key.as_bytes())
                .with_context(|| format!("delete old index {old_key}"))?;
        }
    }
    Ok(())
}

/// Classify an edge by looking up the post-migration (or current) stream
/// of its source + target entities. An edge is `AlreadyCorrect` when both
/// endpoints share a stream AND that stream equals `edge.stream_id`.
enum EdgeCategory {
    AlreadyCorrect,
    /// Both endpoints in the same stream, but `edge.stream_id` disagrees.
    MisAligned {
        new_stream: String,
    },
    /// Source or target entity is missing — skip.
    OrphanEntity,
    /// Endpoints in different streams — cross-stream policy not in scope.
    CrossStream,
}

/// Look up an entity's current stream_id directly (post-entity-migration
/// state, since entities are rewritten in-place first).
fn entity_stream_by_id_33b(db: &DB, entity_id: &str) -> Result<Option<String>> {
    let key = format!("graph:entity:{entity_id}");
    let Some(bytes) = db
        .get(key.as_bytes())
        .with_context(|| format!("read entity {key}"))?
    else {
        return Ok(None);
    };
    let v: Value =
        serde_json::from_slice(&bytes).context("deserialize entity for edge stream lookup")?;
    Ok(v.get("stream_id")
        .and_then(Value::as_str)
        .map(str::to_string))
}

fn classify_edge(db: &DB, edge: &Value) -> Result<EdgeCategory> {
    let current_stream = edge
        .get("stream_id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let src_id = edge
        .get("source_entity_id")
        .and_then(Value::as_str)
        .unwrap_or("");
    let tgt_id = edge
        .get("target_entity_id")
        .and_then(Value::as_str)
        .unwrap_or("");
    let Some(src_stream) = entity_stream_by_id_33b(db, src_id)? else {
        return Ok(EdgeCategory::OrphanEntity);
    };
    let Some(tgt_stream) = entity_stream_by_id_33b(db, tgt_id)? else {
        return Ok(EdgeCategory::OrphanEntity);
    };
    if src_stream != tgt_stream {
        return Ok(EdgeCategory::CrossStream);
    }
    if src_stream == current_stream {
        Ok(EdgeCategory::AlreadyCorrect)
    } else {
        Ok(EdgeCategory::MisAligned {
            new_stream: src_stream,
        })
    }
}

fn rewrite_edge(db: &DB, edge_json: &mut Value, key: &[u8], new_stream: &str) -> Result<()> {
    if let Some(obj) = edge_json.as_object_mut() {
        obj.insert(
            "stream_id".to_string(),
            Value::String(new_stream.to_string()),
        );
        obj.insert(
            "updated_at".to_string(),
            Value::Number(serde_json::Number::from(now_unix_secs())),
        );
    }
    let bytes = serde_json::to_vec(edge_json).context("serialize rewritten edge")?;
    db.put(key, &bytes).context("put rewritten edge")?;
    Ok(())
}

/// Scan all entities + edges, classify, and (on commit) rewrite mis-aligned
/// rows. Returns stats + samples for the manifest.
/// Combined output of a scan (and optional rewrite) pass. Grouped in a
/// struct to avoid a clippy `type_complexity` sprawl on the function
/// signature.
#[derive(Debug, Default)]
struct GraphScanOutput {
    stats: GraphMigrationStats,
    rewritten: Vec<EntityRewriteSample>,
    orphan: Vec<EntitySkipSample>,
    mixed: Vec<EntitySkipSample>,
}

#[allow(clippy::too_many_lines)]
fn scan_and_rewrite_graph(db: &DB, commit: bool) -> Result<GraphScanOutput> {
    let mut stats = GraphMigrationStats::default();
    let mut rewritten_samples: Vec<EntityRewriteSample> = Vec::new();
    let mut mixed_samples: Vec<EntitySkipSample> = Vec::new();
    let mut orphan_samples: Vec<EntitySkipSample> = Vec::new();

    // Phase pass 1 — entities.
    {
        let prefix = b"graph:entity:" as &[u8];
        let iter = db.iterator(IteratorMode::From(prefix, rocksdb::Direction::Forward));
        for item in iter {
            let (key, value) = item.context("iterate graph entities")?;
            if !key.starts_with(prefix) {
                break;
            }
            stats.entities_scanned += 1;
            let Ok(mut entity) = serde_json::from_slice::<Value>(&value) else {
                continue;
            };
            let category = classify_entity(db, &entity)?;
            let id = entity
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let canonical = entity
                .get("canonical_name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let current_stream = entity
                .get("stream_id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            match category {
                EntityCategory::AlreadyCorrect => {
                    stats.entities_already_correct += 1;
                }
                EntityCategory::MisAligned { new_stream } => {
                    stats.entities_mis_aligned += 1;
                    if rewritten_samples.len() < 100 {
                        rewritten_samples.push(EntityRewriteSample {
                            id: id.clone(),
                            canonical_name: canonical.clone(),
                            old_stream: current_stream.clone(),
                            new_stream: new_stream.clone(),
                        });
                    }
                    if commit {
                        rewrite_entity(db, &mut entity, &key, &new_stream)?;
                    }
                }
                EntityCategory::Orphan => {
                    stats.entities_orphan += 1;
                    if orphan_samples.len() < 100 {
                        orphan_samples.push(EntitySkipSample {
                            id: id.clone(),
                            canonical_name: canonical.clone(),
                            stream_id: current_stream.clone(),
                            chunk_streams: Vec::new(),
                        });
                    }
                }
                EntityCategory::MixedStreams { chunk_streams } => {
                    stats.entities_mixed_streams += 1;
                    if mixed_samples.len() < 100 {
                        mixed_samples.push(EntitySkipSample {
                            id: id.clone(),
                            canonical_name: canonical.clone(),
                            stream_id: current_stream.clone(),
                            chunk_streams,
                        });
                    }
                }
            }
        }
    }

    // Pass 2 — edges. Runs after entities in commit mode so
    // `entity_stream_by_id_33b` sees post-migration entity streams.
    {
        let prefix = b"graph:edge:" as &[u8];
        let iter = db.iterator(IteratorMode::From(prefix, rocksdb::Direction::Forward));
        for item in iter {
            let (key, value) = item.context("iterate graph edges")?;
            if !key.starts_with(prefix) {
                break;
            }
            stats.edges_scanned += 1;
            let Ok(mut edge) = serde_json::from_slice::<Value>(&value) else {
                continue;
            };
            let category = classify_edge(db, &edge)?;
            match category {
                EdgeCategory::AlreadyCorrect => {
                    stats.edges_already_correct += 1;
                }
                EdgeCategory::MisAligned { new_stream } => {
                    stats.edges_mis_aligned += 1;
                    if commit {
                        rewrite_edge(db, &mut edge, &key, &new_stream)?;
                    }
                }
                EdgeCategory::OrphanEntity => {
                    stats.edges_orphan_entity += 1;
                }
                EdgeCategory::CrossStream => {
                    stats.edges_cross_stream += 1;
                }
            }
        }
    }

    Ok(GraphScanOutput {
        stats,
        rewritten: rewritten_samples,
        orphan: orphan_samples,
        mixed: mixed_samples,
    })
}

/// Write a plain-text manifest (0600) summarising scan + rewrite results.
/// Format is inspectable by hand; not JSON because operator copy-pastes
/// into review notes.
fn write_graph_migration_manifest(
    dir: &Path,
    commit: bool,
    stats: &GraphMigrationStats,
    rewritten: &[EntityRewriteSample],
    orphan: &[EntitySkipSample],
    mixed: &[EntitySkipSample],
) -> Result<PathBuf33b> {
    static MANIFEST_SEQ_33B: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = MANIFEST_SEQ_33B.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let ts = now_unix_secs();
    let pid = std::process::id();
    let path = dir.join(format!(
        "graph-migration-manifest-{ts}-p{pid}-s{seq:04}.txt"
    ));
    let mut body = String::new();
    body.push_str(&format!(
        "# Graph entity stream migration manifest — {}\n",
        now_ts_readable()
    ));
    body.push_str(&format!(
        "# RUN MODE: {}\n",
        if commit { "--commit" } else { "dry-run" }
    ));
    body.push_str("# Cycle /33b — C1 leftover graph stream_id re-stamp\n\n");

    body.push_str("SUMMARY:\n");
    body.push_str(&format!(
        "  entities_scanned                 : {}\n",
        stats.entities_scanned
    ));
    body.push_str(&format!(
        "  entities_rewritten               : {}\n",
        stats.entities_mis_aligned
    ));
    body.push_str(&format!(
        "  entities_skipped_already_correct : {}\n",
        stats.entities_already_correct
    ));
    body.push_str(&format!(
        "  entities_skipped_orphan          : {}\n",
        stats.entities_orphan
    ));
    body.push_str(&format!(
        "  entities_skipped_mixed_streams   : {}\n",
        stats.entities_mixed_streams
    ));
    body.push_str(&format!(
        "  edges_scanned                    : {}\n",
        stats.edges_scanned
    ));
    body.push_str(&format!(
        "  edges_rewritten                  : {}\n",
        stats.edges_mis_aligned
    ));
    body.push_str(&format!(
        "  edges_skipped_already_correct    : {}\n",
        stats.edges_already_correct
    ));
    body.push_str(&format!(
        "  edges_skipped_orphan_entity      : {}\n",
        stats.edges_orphan_entity
    ));
    body.push_str(&format!(
        "  edges_skipped_cross_stream       : {}\n\n",
        stats.edges_cross_stream
    ));

    body.push_str("[ENTITIES REWRITTEN]\n");
    if rewritten.is_empty() {
        body.push_str("  (none)\n");
    } else {
        for s in rewritten {
            body.push_str(&format!(
                "  id: {}  old_stream: {}  new_stream: {}  canonical_name: {}\n",
                s.id, s.old_stream, s.new_stream, s.canonical_name
            ));
        }
    }
    body.push('\n');

    body.push_str("[ENTITIES SKIPPED — MIXED CHUNK STREAMS]\n");
    if mixed.is_empty() {
        body.push_str("  (none)\n");
    } else {
        for s in mixed {
            body.push_str(&format!(
                "  id: {}  stream_id: {}  chunk_streams: {:?}  canonical_name: {}\n",
                s.id, s.stream_id, s.chunk_streams, s.canonical_name
            ));
        }
    }
    body.push('\n');

    body.push_str("[ENTITIES SKIPPED — ORPHAN (no resolvable chunks)]\n");
    if orphan.is_empty() {
        body.push_str("  (none)\n");
    } else {
        for s in orphan {
            body.push_str(&format!(
                "  id: {}  stream_id: {}  canonical_name: {}\n",
                s.id, s.stream_id, s.canonical_name
            ));
        }
    }
    body.push('\n');

    write_owner_only(&path, body.as_bytes())?;
    Ok(path)
}

fn cmd_migrate_graph_entity_streams(args: &[String]) -> Result<()> {
    let commit = args.iter().any(|a| a == "--commit");
    let db_path = resolve_db_path(args);
    let manifest_dir = manifest_dir_from_args(args);

    println!("loomem-migrate --migrate-graph-entity-streams");
    println!("  db_path : {db_path}");
    println!(
        "  mode    : {}",
        if commit {
            "COMMIT"
        } else {
            "DRY RUN (no writes)"
        }
    );

    let db = open_db(&db_path)?;

    // On commit: backup checkpoint BEFORE any mutation. On dry-run: no
    // checkpoint — no writes happen.
    //
    // Backup dir name includes pid + atomic seq so parallel test runs (and
    // the very unlikely concurrent-operator scenario) do not collide inside
    // the same second. Production operator runs this sequentially; the
    // extra suffix is cosmetic there.
    if commit {
        static BACKUP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = BACKUP_SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let pid = std::process::id();
        let backup_dir_name = format!(
            "data-backup-pre-graph-migration-{}-p{pid}-s{seq:04}",
            now_ts_readable()
        );
        let backup_path = Path::new(&db_path)
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(&backup_dir_name);
        println!();
        println!("Creating RocksDB checkpoint at {}", backup_path.display());
        checkpoint_backup(&db, &backup_path)?;
    }

    let scan = scan_and_rewrite_graph(&db, commit)?;
    let stats = &scan.stats;
    let rewritten = &scan.rewritten;
    let orphan = &scan.orphan;
    let mixed = &scan.mixed;

    println!();
    println!("ENTITIES:");
    println!("  scanned                 : {}", stats.entities_scanned);
    println!(
        "  already_correct         : {}",
        stats.entities_already_correct
    );
    println!(
        "  mis_aligned ({})        : {}",
        if commit { "rewritten" } else { "to migrate" },
        stats.entities_mis_aligned
    );
    println!("  orphan (skipped)        : {}", stats.entities_orphan);
    println!(
        "  mixed_streams (skipped) : {}",
        stats.entities_mixed_streams
    );
    println!("EDGES:");
    println!("  scanned                 : {}", stats.edges_scanned);
    println!(
        "  already_correct         : {}",
        stats.edges_already_correct
    );
    println!(
        "  mis_aligned ({})        : {}",
        if commit { "rewritten" } else { "to migrate" },
        stats.edges_mis_aligned
    );
    println!("  orphan_entity (skipped) : {}", stats.edges_orphan_entity);
    println!("  cross_stream (skipped)  : {}", stats.edges_cross_stream);

    let manifest_path =
        write_graph_migration_manifest(&manifest_dir, commit, stats, rewritten, orphan, mixed)?;
    println!();
    println!("Manifest: {}", manifest_path.display());

    if !commit {
        println!();
        println!("Run with --commit to apply changes.");
    }

    // Cycle /49: signal Tantivy rebuild needed when graph entity/edge stream
    // tags were rewritten. Server boot will rebuild Tantivy from RocksDB.
    if commit && (stats.entities_mis_aligned > 0 || stats.edges_mis_aligned > 0) {
        db.put(TANTIVY_REBUILD_FLAG_KEY, b"1")
            .context("Failed to set meta:tantivy_rebuild_needed flag")?;
        println!("Set meta:tantivy_rebuild_needed=1 — server boot will rebuild Tantivy");
    }

    Ok(())
}

fn cmd_validate_graph_entity_streams(args: &[String]) -> Result<()> {
    let db_path = resolve_db_path(args);
    println!("loomem-migrate --validate-graph-entity-streams");
    println!("  db_path : {db_path}");

    let db = open_db(&db_path)?;
    // Read-only: `scan_and_rewrite_graph(db, commit=false)` does zero writes.
    let scan = scan_and_rewrite_graph(&db, false)?;
    let stats = &scan.stats;

    println!();
    println!("ENTITIES:");
    println!("  scanned         : {}", stats.entities_scanned);
    println!("  already_correct : {}", stats.entities_already_correct);
    println!("  mis_aligned     : {}", stats.entities_mis_aligned);
    println!("  orphan          : {}", stats.entities_orphan);
    println!("  mixed_streams   : {}", stats.entities_mixed_streams);
    println!("EDGES:");
    println!("  scanned         : {}", stats.edges_scanned);
    println!("  already_correct : {}", stats.edges_already_correct);
    println!("  mis_aligned     : {}", stats.edges_mis_aligned);
    println!("  orphan_entity   : {}", stats.edges_orphan_entity);
    println!("  cross_stream    : {}", stats.edges_cross_stream);

    // Non-zero exit when anything needs migration. CI + post-migration audit
    // guard rail.
    if stats.entities_mis_aligned > 0 || stats.edges_mis_aligned > 0 {
        println!();
        println!(
            "AUDIT FAIL — {} mis-aligned entities + {} mis-aligned edges. Run --migrate-graph-entity-streams --commit.",
            stats.entities_mis_aligned, stats.edges_mis_aligned
        );
        std::process::exit(2);
    }

    println!();
    println!("AUDIT PASS — graph entity + edge streams aligned with chunks.");
    Ok(())
}

// ── helpers ────────────────────────────────────────────────────────────────────

fn resolve_db_path(args: &[String]) -> String {
    let idx = args.iter().position(|a| a == "--db");
    if let Some(i) = idx {
        if let Some(path) = args.get(i + 1) {
            return path.clone();
        }
    }
    "./data/rocksdb".to_string()
}

fn open_db(db_path: &str) -> Result<DB> {
    let mut opts = Options::default();
    opts.set_max_open_files(100);
    opts.create_if_missing(true);

    let cf_names = DB::list_cf(&opts, db_path).unwrap_or_default();
    let cfs: Vec<_> = cf_names
        .iter()
        .map(|name| rocksdb::ColumnFamilyDescriptor::new(name, Options::default()))
        .collect();

    if cfs.is_empty() {
        DB::open(&opts, db_path).context("Failed to open RocksDB")
    } else {
        DB::open_cf_descriptors(&opts, db_path, cfs).context("Failed to open RocksDB with CFs")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn unique_test_dir(name: &str) -> PathBuf {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("loomem-migrate-test-{}-{}", name, ts));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn test_resolve_db_path_uses_db_flag_not_hardcoded_path() {
        let args = vec![
            "loomem-migrate".to_string(),
            "--validate-graph-entity-streams".to_string(),
            "--db".to_string(),
            "/tmp/custom-db".to_string(),
        ];
        let path = resolve_db_path(&args);
        assert_eq!(path, "/tmp/custom-db");
    }

    // ── cycle/33b — graph entity stream_id migration ────────────────────

    /// Seed a chunk in the default CF under the loomem-core key format
    /// (`chunk:L{level}:{id}`). Uses `Value` not a concrete struct so the
    /// test matches loomem-migrate's untyped JSON iteration pattern.
    fn seed_chunk_33b(db: &DB, id: &str, stream: &str, level: i32) {
        let chunk = serde_json::json!({
            "id": id,
            "content": "c",
            "stream": stream,
            "level": level,
            "score": 0.5,
            "timestamp": 1_000_000_u64,
            "consolidated": false,
            "dormant": false,
            "in_progress": false,
            "is_latest": true,
            "version": 1_u32,
        });
        let bytes = serde_json::to_vec(&chunk).unwrap();
        let key = format!("chunk:L{level}:{id}");
        db.put(key.as_bytes(), &bytes).unwrap();
    }

    fn seed_entity_33b(
        db: &DB,
        id: &str,
        canonical_name: &str,
        aliases: &[&str],
        chunk_ids: &[&str],
        stream_id: &str,
    ) {
        let entity = serde_json::json!({
            "id": id,
            "canonical_name": canonical_name,
            "entity_type": "concept",
            "aliases": aliases,
            "chunk_ids": chunk_ids,
            "stream_id": stream_id,
            "created_at": 1_000_000_u64,
            "updated_at": 1_000_000_u64,
        });
        let bytes = serde_json::to_vec(&entity).unwrap();
        db.put(format!("graph:entity:{id}").as_bytes(), &bytes)
            .unwrap();

        // Seed per-stream name + alias indices the same way graph.rs does.
        let sp = graph_stream_prefix_33b(stream_id);
        let canon_lower = canonical_name.to_lowercase();
        db.put(format!("{sp}name:{canon_lower}").as_bytes(), id.as_bytes())
            .unwrap();
        db.put(format!("{sp}alias:{canon_lower}").as_bytes(), id.as_bytes())
            .unwrap();
        for a in aliases {
            db.put(
                format!("{sp}alias:{}", a.to_lowercase()).as_bytes(),
                id.as_bytes(),
            )
            .unwrap();
        }
    }

    fn seed_edge_33b(db: &DB, id: &str, src: &str, tgt: &str, relation: &str, stream_id: &str) {
        let edge = serde_json::json!({
            "id": id,
            "source_entity_id": src,
            "target_entity_id": tgt,
            "relation_type": relation,
            "chunk_ids": [],
            "stream_id": stream_id,
            "created_at": 1_000_000_u64,
            "updated_at": 1_000_000_u64,
        });
        let bytes = serde_json::to_vec(&edge).unwrap();
        db.put(format!("graph:edge:{id}").as_bytes(), &bytes)
            .unwrap();
        db.put(format!("graph:adj:{src}:{id}").as_bytes(), tgt.as_bytes())
            .unwrap();
        db.put(format!("graph:radj:{tgt}:{id}").as_bytes(), src.as_bytes())
            .unwrap();
    }

    fn read_entity_33b(db: &DB, id: &str) -> Value {
        let bytes = db
            .get(format!("graph:entity:{id}").as_bytes())
            .unwrap()
            .expect("entity present");
        serde_json::from_slice(&bytes).unwrap()
    }

    fn read_edge_33b(db: &DB, id: &str) -> Value {
        let bytes = db
            .get(format!("graph:edge:{id}").as_bytes())
            .unwrap()
            .expect("edge present");
        serde_json::from_slice(&bytes).unwrap()
    }

    fn args_for_33b(db_path: &str, manifest_dir: &str, commit: bool) -> Vec<String> {
        let mut v = vec![
            "loomem-migrate".to_string(),
            "--migrate-graph-entity-streams".to_string(),
            "--db".to_string(),
            db_path.to_string(),
            "--manifest-dir".to_string(),
            manifest_dir.to_string(),
        ];
        if commit {
            v.push("--commit".to_string());
        }
        v
    }

    #[test]
    fn cycle33b_dry_run_no_op_on_misaligned_state() {
        let dir = unique_test_dir("33b-dry-run");
        let db_path = dir.join("rocksdb").to_str().unwrap().to_string();
        let manifest_dir = dir.to_str().unwrap().to_string();
        {
            let db = open_db(&db_path).unwrap();
            // mis-aligned entity: stream_id=old, chunk in __shared_main__
            seed_chunk_33b(&db, "c1", "__shared_main__", 0);
            seed_entity_33b(&db, "e1", "Acme Bold", &["acme"], &["c1"], "legacy-old");
        }

        cmd_migrate_graph_entity_streams(&args_for_33b(&db_path, &manifest_dir, false)).unwrap();

        // Re-open DB and verify entity.stream_id unchanged.
        let db2 = open_db(&db_path).unwrap();
        let e = read_entity_33b(&db2, "e1");
        assert_eq!(
            e.get("stream_id").and_then(Value::as_str),
            Some("legacy-old")
        );

        // Old index still present, new index absent (zero writes).
        assert!(db2
            .get(b"graph:s:legacy-old:name:acme bold")
            .unwrap()
            .is_some());
        assert!(db2
            .get(b"graph:s:__shared_main__:name:acme bold")
            .unwrap()
            .is_none());
    }

    #[test]
    fn cycle33b_already_correct_skipped() {
        let dir = unique_test_dir("33b-correct-skip");
        let db_path = dir.join("rocksdb").to_str().unwrap().to_string();
        let manifest_dir = dir.to_str().unwrap().to_string();
        {
            let db = open_db(&db_path).unwrap();
            seed_chunk_33b(&db, "c1", "__shared_main__", 0);
            seed_entity_33b(&db, "e1", "Shared Thing", &[], &["c1"], "__shared_main__");
        }

        cmd_migrate_graph_entity_streams(&args_for_33b(&db_path, &manifest_dir, true)).unwrap();

        let db2 = open_db(&db_path).unwrap();
        let e = read_entity_33b(&db2, "e1");
        // Stream unchanged.
        assert_eq!(
            e.get("stream_id").and_then(Value::as_str),
            Some("__shared_main__")
        );
        // updated_at MUST NOT have been bumped (no rewrite fired).
        assert_eq!(
            e.get("updated_at").and_then(Value::as_u64),
            Some(1_000_000_u64)
        );
    }

    #[test]
    fn cycle33b_misaligned_rewritten_on_commit() {
        let dir = unique_test_dir("33b-misaligned-rewrite");
        let db_path = dir.join("rocksdb").to_str().unwrap().to_string();
        let manifest_dir = dir.to_str().unwrap().to_string();
        {
            let db = open_db(&db_path).unwrap();
            // Pre-C1 admin private stream; chunk migrated to __shared_main__ by C1.
            seed_chunk_33b(&db, "c_bold", "__shared_main__", 1);
            seed_entity_33b(
                &db,
                "e_bold",
                "Acme Bold",
                &["acme", "bold"],
                &["c_bold"],
                "legacy-owner",
            );
        }

        cmd_migrate_graph_entity_streams(&args_for_33b(&db_path, &manifest_dir, true)).unwrap();

        let db2 = open_db(&db_path).unwrap();
        let e = read_entity_33b(&db2, "e_bold");
        assert_eq!(
            e.get("stream_id").and_then(Value::as_str),
            Some("__shared_main__")
        );
        // updated_at bumped.
        let bumped = e
            .get("updated_at")
            .and_then(Value::as_u64)
            .expect("updated_at");
        assert!(bumped >= 1_000_000);

        // Indices moved.
        assert!(db2
            .get(b"graph:s:legacy-owner:name:acme bold")
            .unwrap()
            .is_none());
        assert!(db2
            .get(b"graph:s:__shared_main__:name:acme bold")
            .unwrap()
            .is_some());
        // Alias indices moved (canonical-as-alias + explicit aliases).
        assert!(db2
            .get(b"graph:s:legacy-owner:alias:acme")
            .unwrap()
            .is_none());
        assert!(db2
            .get(b"graph:s:__shared_main__:alias:acme")
            .unwrap()
            .is_some());
        assert!(db2
            .get(b"graph:s:__shared_main__:alias:bold")
            .unwrap()
            .is_some());
        assert!(db2
            .get(b"graph:s:__shared_main__:alias:acme bold")
            .unwrap()
            .is_some());

        // Backup checkpoint dir exists alongside the db.
        let parent = Path::new(&db_path)
            .parent()
            .unwrap_or_else(|| Path::new("."));
        let found_checkpoint = std::fs::read_dir(parent)
            .unwrap()
            .filter_map(std::result::Result::ok)
            .any(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("data-backup-pre-graph-migration-")
            });
        assert!(found_checkpoint, "backup checkpoint dir expected");

        // Manifest was written with 0600.
        let manifest = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(std::result::Result::ok)
            .find(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("graph-migration-manifest-")
            })
            .expect("manifest file present");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let meta = std::fs::metadata(manifest.path()).unwrap();
            let mode = meta.permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "manifest must be 0600 (owner-only)");
        }
        #[cfg(not(unix))]
        {
            let _ = manifest; // unused on non-unix
        }
    }

    #[test]
    fn cycle33b_orphan_no_chunks_skipped() {
        let dir = unique_test_dir("33b-orphan");
        let db_path = dir.join("rocksdb").to_str().unwrap().to_string();
        let manifest_dir = dir.to_str().unwrap().to_string();
        {
            let db = open_db(&db_path).unwrap();
            // No chunks at all. Orphan.
            seed_entity_33b(&db, "e_orphan", "Ghost", &[], &[], "legacy-old");
        }
        cmd_migrate_graph_entity_streams(&args_for_33b(&db_path, &manifest_dir, true)).unwrap();

        let db2 = open_db(&db_path).unwrap();
        let e = read_entity_33b(&db2, "e_orphan");
        assert_eq!(
            e.get("stream_id").and_then(Value::as_str),
            Some("legacy-old")
        );
        assert_eq!(
            e.get("updated_at").and_then(Value::as_u64),
            Some(1_000_000_u64)
        );
    }

    #[test]
    fn cycle33b_orphan_chunk_refs_missing_skipped() {
        let dir = unique_test_dir("33b-orphan-missing");
        let db_path = dir.join("rocksdb").to_str().unwrap().to_string();
        let manifest_dir = dir.to_str().unwrap().to_string();
        {
            let db = open_db(&db_path).unwrap();
            // chunk_ids references chunks that don't exist.
            seed_entity_33b(&db, "e_dangle", "Dangle", &[], &["c_missing"], "legacy-old");
        }
        cmd_migrate_graph_entity_streams(&args_for_33b(&db_path, &manifest_dir, true)).unwrap();

        let db2 = open_db(&db_path).unwrap();
        let e = read_entity_33b(&db2, "e_dangle");
        assert_eq!(
            e.get("stream_id").and_then(Value::as_str),
            Some("legacy-old")
        );
    }

    #[test]
    fn cycle33b_mixed_streams_skipped_and_flagged() {
        let dir = unique_test_dir("33b-mixed");
        let db_path = dir.join("rocksdb").to_str().unwrap().to_string();
        let manifest_dir = dir.to_str().unwrap().to_string();
        {
            let db = open_db(&db_path).unwrap();
            // Chunks in two different streams — heuristic forbidden, must skip.
            seed_chunk_33b(&db, "c_a", "__shared_main__", 0);
            seed_chunk_33b(&db, "c_b", "legacy-other", 0);
            seed_entity_33b(
                &db,
                "e_mixed",
                "Multistream",
                &[],
                &["c_a", "c_b"],
                "legacy-old",
            );
        }
        cmd_migrate_graph_entity_streams(&args_for_33b(&db_path, &manifest_dir, true)).unwrap();

        let db2 = open_db(&db_path).unwrap();
        let e = read_entity_33b(&db2, "e_mixed");
        // Stream unchanged.
        assert_eq!(
            e.get("stream_id").and_then(Value::as_str),
            Some("legacy-old")
        );
        // Manifest mentions the skip category.
        let manifest = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(std::result::Result::ok)
            .find(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("graph-migration-manifest-")
            })
            .expect("manifest");
        let body = std::fs::read_to_string(manifest.path()).unwrap();
        assert!(
            body.contains("MIXED CHUNK STREAMS"),
            "manifest must flag mixed-streams category: {body}"
        );
        assert!(
            body.contains("e_mixed"),
            "manifest must list the skipped entity id"
        );
    }

    #[test]
    fn cycle33b_idempotent_second_commit_zero_delta() {
        let dir = unique_test_dir("33b-idem");
        let db_path = dir.join("rocksdb").to_str().unwrap().to_string();
        let manifest_dir = dir.to_str().unwrap().to_string();
        {
            let db = open_db(&db_path).unwrap();
            seed_chunk_33b(&db, "c1", "__shared_main__", 2);
            seed_entity_33b(&db, "e1", "Foo", &[], &["c1"], "legacy-old");
        }

        // First commit: rewrites e1.
        cmd_migrate_graph_entity_streams(&args_for_33b(&db_path, &manifest_dir, true)).unwrap();
        let stats_first = {
            let db = open_db(&db_path).unwrap();
            scan_and_rewrite_graph(&db, false).unwrap().stats
        };
        assert_eq!(
            stats_first.entities_mis_aligned, 0,
            "should be 0 after first commit"
        );
        assert_eq!(stats_first.entities_already_correct, 1);

        // Second commit: zero rewrites (audit-re-scan confirms).
        cmd_migrate_graph_entity_streams(&args_for_33b(&db_path, &manifest_dir, true)).unwrap();
        let stats_second = {
            let db = open_db(&db_path).unwrap();
            scan_and_rewrite_graph(&db, false).unwrap().stats
        };
        assert_eq!(stats_second.entities_mis_aligned, 0);
        assert_eq!(stats_second.entities_already_correct, 1);
    }

    #[test]
    fn cycle33b_edge_migration_restamped_based_on_entity_streams() {
        let dir = unique_test_dir("33b-edge-restamp");
        let db_path = dir.join("rocksdb").to_str().unwrap().to_string();
        let manifest_dir = dir.to_str().unwrap().to_string();
        {
            let db = open_db(&db_path).unwrap();
            seed_chunk_33b(&db, "c1", "__shared_main__", 0);
            seed_chunk_33b(&db, "c2", "__shared_main__", 0);
            seed_entity_33b(&db, "src", "Src", &[], &["c1"], "legacy-old");
            seed_entity_33b(&db, "tgt", "Tgt", &[], &["c2"], "legacy-old");
            seed_edge_33b(&db, "edge1", "src", "tgt", "about", "legacy-old");
        }

        cmd_migrate_graph_entity_streams(&args_for_33b(&db_path, &manifest_dir, true)).unwrap();

        let db2 = open_db(&db_path).unwrap();
        // Entities migrated.
        assert_eq!(
            read_entity_33b(&db2, "src")
                .get("stream_id")
                .and_then(Value::as_str),
            Some("__shared_main__")
        );
        assert_eq!(
            read_entity_33b(&db2, "tgt")
                .get("stream_id")
                .and_then(Value::as_str),
            Some("__shared_main__")
        );
        // Edge re-stamped to the aligned entity stream.
        let e = read_edge_33b(&db2, "edge1");
        assert_eq!(
            e.get("stream_id").and_then(Value::as_str),
            Some("__shared_main__")
        );
    }

    #[test]
    fn cycle33b_edge_cross_stream_skipped() {
        let dir = unique_test_dir("33b-edge-cross");
        let db_path = dir.join("rocksdb").to_str().unwrap().to_string();
        let manifest_dir = dir.to_str().unwrap().to_string();
        {
            let db = open_db(&db_path).unwrap();
            // src in shared post-migration, tgt in private — cross-stream.
            seed_chunk_33b(&db, "c_src", "__shared_main__", 0);
            seed_chunk_33b(&db, "c_tgt", "__user_xyz__", 0);
            seed_entity_33b(&db, "src", "S", &[], &["c_src"], "__shared_main__");
            seed_entity_33b(&db, "tgt", "T", &[], &["c_tgt"], "__user_xyz__");
            seed_edge_33b(&db, "e_cross", "src", "tgt", "linked", "legacy-old");
        }
        cmd_migrate_graph_entity_streams(&args_for_33b(&db_path, &manifest_dir, true)).unwrap();

        let db2 = open_db(&db_path).unwrap();
        // Both entities already correct at seed time.
        // Edge NOT rewritten (cross-stream).
        let e = read_edge_33b(&db2, "e_cross");
        assert_eq!(
            e.get("stream_id").and_then(Value::as_str),
            Some("legacy-old")
        );

        // Validator reports cross_stream in the same run.
        let stats = scan_and_rewrite_graph(&db2, false).unwrap().stats;
        assert_eq!(stats.edges_cross_stream, 1);
    }

    #[test]
    fn cycle33b_edge_orphan_entity_skipped() {
        let dir = unique_test_dir("33b-edge-orphan");
        let db_path = dir.join("rocksdb").to_str().unwrap().to_string();
        let manifest_dir = dir.to_str().unwrap().to_string();
        {
            let db = open_db(&db_path).unwrap();
            // Edge references entities that don't exist.
            seed_edge_33b(&db, "e_orphan", "ghost_a", "ghost_b", "x", "legacy-old");
        }
        cmd_migrate_graph_entity_streams(&args_for_33b(&db_path, &manifest_dir, true)).unwrap();

        let db2 = open_db(&db_path).unwrap();
        let e = read_edge_33b(&db2, "e_orphan");
        assert_eq!(
            e.get("stream_id").and_then(Value::as_str),
            Some("legacy-old")
        );
        let stats = scan_and_rewrite_graph(&db2, false).unwrap().stats;
        assert_eq!(stats.edges_orphan_entity, 1);
    }

    #[test]
    fn cycle33b_backup_checkpoint_only_created_on_commit() {
        let dir = unique_test_dir("33b-backup-dry");
        let db_path = dir.join("rocksdb").to_str().unwrap().to_string();
        let manifest_dir = dir.to_str().unwrap().to_string();
        {
            let db = open_db(&db_path).unwrap();
            seed_chunk_33b(&db, "c1", "__shared_main__", 0);
            seed_entity_33b(&db, "e1", "X", &[], &["c1"], "legacy-old");
        }

        // Dry-run: no backup dir.
        cmd_migrate_graph_entity_streams(&args_for_33b(&db_path, &manifest_dir, false)).unwrap();
        let parent = Path::new(&db_path)
            .parent()
            .unwrap_or_else(|| Path::new("."));
        let any_backup_after_dry = std::fs::read_dir(parent)
            .unwrap()
            .filter_map(std::result::Result::ok)
            .any(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("data-backup-pre-graph-migration-")
            });
        assert!(!any_backup_after_dry, "dry-run must not create backup");
    }

    #[test]
    fn cycle33b_manifest_contains_category_sections_and_summary() {
        let dir = unique_test_dir("33b-manifest-fmt");
        let db_path = dir.join("rocksdb").to_str().unwrap().to_string();
        let manifest_dir = dir.to_str().unwrap().to_string();
        {
            let db = open_db(&db_path).unwrap();
            // mis-aligned
            seed_chunk_33b(&db, "c1", "__shared_main__", 0);
            seed_entity_33b(&db, "e_mis", "M", &[], &["c1"], "legacy-old");
            // already correct
            seed_chunk_33b(&db, "c2", "__shared_main__", 0);
            seed_entity_33b(&db, "e_ok", "O", &[], &["c2"], "__shared_main__");
            // orphan
            seed_entity_33b(&db, "e_orph", "Or", &[], &[], "legacy-old");
        }
        cmd_migrate_graph_entity_streams(&args_for_33b(&db_path, &manifest_dir, true)).unwrap();

        let manifest = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(std::result::Result::ok)
            .find(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("graph-migration-manifest-")
            })
            .expect("manifest");
        let body = std::fs::read_to_string(manifest.path()).unwrap();

        assert!(body.contains("SUMMARY:"));
        assert!(body.contains("entities_scanned"));
        assert!(body.contains("entities_rewritten"));
        assert!(body.contains("edges_scanned"));
        assert!(body.contains("[ENTITIES REWRITTEN]"));
        assert!(body.contains("[ENTITIES SKIPPED — MIXED CHUNK STREAMS]"));
        assert!(body.contains("[ENTITIES SKIPPED — ORPHAN"));
        assert!(body.contains("# RUN MODE: --commit"));
        // Cycle /33b header.
        assert!(body.contains("Cycle /33b"));
    }
}
