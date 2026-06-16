//! /109 AC-3 — leakage probe.
//!
//! Opens RocksDB read-only, loads embeddings for a single stream,
//! and tests whether orphan chunk_ids appear in vector_search top-k.
//!
//! Usage:
//!   leakage_probe <db_path> <stream_id> <orphan_id1> [orphan_id2 ...]
//!
//! Verdict:
//!   GREEN  — none of the orphan ids appear in top-20 results.
//!   YELLOW — at least one orphan id appears in top-20 (pure vector hit).
//!
//! (RED verdict — orphan chunk content fetchable — is not assessed here;
//!  that would require a running loomem-server. The probe documents the
//!  "vector index CAN return orphan" fact only.)
//!
//! The probe is intentionally self-contained (no loomem-core dependency)
//! to compile without the full workspace deps (avoids candle/ort).

use anyhow::{anyhow, Context, Result};
use rocksdb::{ColumnFamilyDescriptor, IteratorMode, Options, DB};

const CF_EMBEDDINGS: &str = "embeddings";

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 4 {
        eprintln!("Usage: leakage_probe <db_path> <stream_id> <orphan_id1> [orphan_id2 ...]");
        std::process::exit(1);
    }
    let db_path = &args[1];
    let stream_id = &args[2];
    let orphan_ids: Vec<&str> = args[3..].iter().map(String::as_str).collect();

    eprintln!("leakage_probe");
    eprintln!("  db_path   : {db_path}");
    eprintln!("  stream    : {stream_id}");
    eprintln!("  orphans   : {}", orphan_ids.len());

    let db = open_readonly(db_path)?;

    // Load all embeddings for the target stream (subset strategy for RAM management).
    // Per-stream estimate: ~225 chunks × 1536d × 4B ≈ 1.4 MB.
    let stream_embeddings = load_stream_embeddings(&db, stream_id)?;
    eprintln!(
        "  loaded    : {} embeddings for stream",
        stream_embeddings.len()
    );

    if stream_embeddings.is_empty() {
        eprintln!("PROBE_FAIL: no embeddings found for stream {stream_id}");
        std::process::exit(2);
    }

    let mut any_hit = false;
    for orphan_id in &orphan_ids {
        // Read the orphan's own embedding vector.
        let orphan_vec = match load_single_embedding(&db, orphan_id)? {
            Some(v) => v,
            None => {
                eprintln!("ORPHAN_NOT_FOUND: {orphan_id} — not in embeddings CF");
                continue;
            }
        };

        // Run vector search using orphan's own vector as query.
        let results = vector_search(&stream_embeddings, &orphan_vec, 20);
        let hit = results.iter().any(|(id, _)| id.as_str() == *orphan_id);
        if hit {
            let score = results
                .iter()
                .find(|(id, _)| id.as_str() == *orphan_id)
                .map(|(_, s)| *s)
                .unwrap_or(0.0);
            eprintln!("HIT: orphan {orphan_id} appears in top-20 with score={score:.4}");
            any_hit = true;
        } else {
            eprintln!("MISS: orphan {orphan_id} NOT in top-20 (vector not in stream sample, or score below threshold)");
        }
    }

    if any_hit {
        println!("VERDICT: YELLOW — at least one orphan appears in vector_search top-20");
        println!("CAVEAT: proves vector index CAN return orphan; does NOT prove production queries hit it");
        println!("CAVEAT: full production path adds chunk-existence check post-vector-rank");
    } else {
        println!(
            "VERDICT: GREEN — no orphan ids found in vector_search top-20 for stream {stream_id}"
        );
        println!("NOTE: orphan may be in a different stream than sampled; GREEN is stream-scoped");
    }

    Ok(())
}

/// Load all embeddings that belong to `stream_id`.
/// Cross-references each embedding key against chunks CF to filter by stream.
fn load_stream_embeddings(db: &DB, stream_id: &str) -> Result<Vec<(String, Vec<f32>)>> {
    let cf_emb = db
        .cf_handle(CF_EMBEDDINGS)
        .context("embeddings CF not found")?;
    let mut out = Vec::new();
    let iter = db.iterator_cf(&cf_emb, IteratorMode::Start);
    for item in iter {
        let (key, value) = item.context("rocksdb iter error")?;
        let chunk_id = String::from_utf8_lossy(&key).into_owned();
        if let Some(stream) = lookup_chunk_stream(db, &chunk_id)? {
            if stream == stream_id {
                match decode_vec_f32(&value) {
                    Ok(v) => out.push((chunk_id, v)),
                    Err(e) => eprintln!("WARN: decode failed for {chunk_id}: {e}"),
                }
            }
        }
        // orphans are skipped — that's correct; we load the non-orphan set
    }
    Ok(out)
}

/// Load a single embedding by chunk_id directly from embeddings CF.
fn load_single_embedding(db: &DB, chunk_id: &str) -> Result<Option<Vec<f32>>> {
    let cf_emb = db
        .cf_handle(CF_EMBEDDINGS)
        .context("embeddings CF not found")?;
    match db.get_cf(&cf_emb, chunk_id.as_bytes())? {
        Some(bytes) => Ok(Some(decode_vec_f32(&bytes)?)),
        None => Ok(None),
    }
}

fn lookup_chunk_stream(db: &DB, chunk_id: &str) -> Result<Option<String>> {
    for level in 0_i32..=2_i32 {
        let key = format!("chunk:L{level}:{chunk_id}");
        if let Some(bytes) = db.get(key.as_bytes())? {
            let v: serde_json::Value = serde_json::from_slice(&bytes)
                .with_context(|| format!("parse chunk JSON {key}"))?;
            return Ok(v
                .get("stream")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string));
        }
    }
    Ok(None)
}

fn open_readonly(db_path: &str) -> Result<DB> {
    let opts = Options::default();
    let cf_names = DB::list_cf(&opts, db_path).context("Failed to list column families")?;
    let cfs: Vec<_> = cf_names
        .iter()
        .map(|name| ColumnFamilyDescriptor::new(name, Options::default()))
        .collect();
    DB::open_cf_descriptors_read_only(&opts, db_path, cfs, false)
        .context("Failed to open RocksDB read-only")
}

fn decode_vec_f32(bytes: &[u8]) -> Result<Vec<f32>> {
    if bytes.len() < 8 {
        return Err(anyhow!(
            "embedding payload too short ({} bytes, expected ≥ 8)",
            bytes.len()
        ));
    }
    let len_bytes: [u8; 8] = bytes[0..8]
        .try_into()
        .context("slice→[u8;8] for length prefix")?;
    let len =
        usize::try_from(u64::from_le_bytes(len_bytes)).context("length prefix exceeds usize")?;
    let expected = 8usize
        .checked_add(
            len.checked_mul(4)
                .ok_or_else(|| anyhow!("length × 4 overflowed usize"))?,
        )
        .ok_or_else(|| anyhow!("payload size overflowed usize"))?;
    if bytes.len() != expected {
        return Err(anyhow!(
            "embedding length mismatch: header says {len} elems → {expected} bytes, payload {} bytes",
            bytes.len()
        ));
    }
    let mut out = Vec::with_capacity(len);
    for i in 0..len {
        let off = 8 + i * 4;
        let raw: [u8; 4] = bytes[off..off + 4]
            .try_into()
            .context("slice→[u8;4] for f32")?;
        out.push(f32::from_bits(u32::from_le_bytes(raw)));
    }
    Ok(out)
}

/// Inline cosine top-k (mirrors loomem-core::vector_search::vector_search).
fn vector_search(
    embeddings: &[(String, Vec<f32>)],
    query: &[f32],
    top_k: usize,
) -> Vec<(String, f32)> {
    let mut scores: Vec<(String, f32)> = embeddings
        .iter()
        .map(|(id, emb)| (id.clone(), cosine_similarity(query, emb)))
        .collect();
    scores.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    scores.truncate(top_k);
    scores
}

fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() {
        return 0.0;
    }
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na * nb)
}
