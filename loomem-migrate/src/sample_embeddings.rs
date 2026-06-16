//! `loomem-migrate --sample-embeddings` — diagnostic dump for the cycle A1
//! embedding anisotropy baseline.
//!
//! Read-only iteration over the `embeddings` column family with
//! cross-reference to the chunk JSON in the default CF for stream
//! attribution. Emits one NDJSON record per emitted sample to stdout;
//! consumed by `scripts/measure-anisotropy.py`.
//!
//! Design notes:
//! - Constant-memory reservoir-A sampling per stream (Algorithm R) with a
//!   per-stream xorshift64 PRNG seeded from `seed XOR FNV1a(stream)`.
//! - Inline bincode-1.x decoder for `Vec<f32>` to avoid a new crate
//!   dependency in `loomem-migrate` (cycle A1 brief: zero new Cargo deps).
//! - DB opened via `open_cf_descriptors_read_only` so the diagnostic can
//!   run while `loomem-server` is live (no log replay, may be stale).

use anyhow::{anyhow, Context, Result};
use rocksdb::{ColumnFamilyDescriptor, IteratorMode, Options, DB};
use serde_json::{json, Value};
use std::collections::HashMap;

const CF_EMBEDDINGS: &str = "embeddings";
const DEFAULT_SAMPLE: usize = 1000;
const DEFAULT_SEED: u64 = 42;

pub fn cmd_sample_embeddings(args: &[String], db_path: &str) -> Result<()> {
    let cfg = Cfg::parse(args)?;
    eprintln!("loomem-migrate --sample-embeddings");
    eprintln!("  db_path     : {db_path}");
    eprintln!("  stream      : {}", cfg.stream_label());
    eprintln!("  sample      : {}", cfg.sample);
    eprintln!("  seed        : {}", cfg.seed);

    let db = open_readonly(db_path)?;
    let mut by_stream: HashMap<String, StreamReservoir> = HashMap::new();
    let stats = collect_samples(&db, &cfg, &mut by_stream)?;

    eprintln!(
        "  scanned     : {} embeddings, {} matched, {} orphan-no-chunk",
        stats.scanned, stats.matched, stats.orphan
    );
    eprintln!("  streams hit : {}", by_stream.len());

    let mut emitted = 0usize;
    for (stream, reservoir) in by_stream {
        for (chunk_id, embedding) in reservoir.into_items() {
            let line = json!({
                "chunk_id": chunk_id,
                "stream_id": stream,
                "embedding": embedding,
            });
            println!("{line}");
            emitted += 1;
        }
    }
    eprintln!("  emitted     : {emitted} samples");
    Ok(())
}

#[derive(Debug)]
struct Cfg {
    stream_filter: Option<String>,
    all_streams: bool,
    sample: usize,
    seed: u64,
}

impl Cfg {
    fn parse(args: &[String]) -> Result<Self> {
        let stream_filter = extract_value(args, "--stream");
        let all_streams = args.iter().any(|a| a == "--all-streams");
        let sample = parse_usize_flag(args, "--sample", DEFAULT_SAMPLE)?;
        let seed = parse_u64_flag(args, "--seed", DEFAULT_SEED)?;
        if !all_streams && stream_filter.is_none() {
            return Err(anyhow!(
                "--sample-embeddings requires --stream <id> or --all-streams"
            ));
        }
        Ok(Self {
            stream_filter,
            all_streams,
            sample,
            seed,
        })
    }

    fn matches(&self, stream: &str) -> bool {
        if self.all_streams {
            return true;
        }
        self.stream_filter.as_deref() == Some(stream)
    }

    fn stream_label(&self) -> String {
        if self.all_streams {
            return "<all-streams>".to_string();
        }
        self.stream_filter
            .clone()
            .unwrap_or_else(|| "<unset>".to_string())
    }
}

#[derive(Default)]
struct ScanStats {
    scanned: u64,
    matched: u64,
    orphan: u64,
}

fn collect_samples(
    db: &DB,
    cfg: &Cfg,
    by_stream: &mut HashMap<String, StreamReservoir>,
) -> Result<ScanStats> {
    let cf_emb = db
        .cf_handle(CF_EMBEDDINGS)
        .context("'embeddings' column family not found")?;
    let mut stats = ScanStats::default();

    let iter = db.iterator_cf(&cf_emb, IteratorMode::Start);
    for item in iter {
        let (key, value) = item.context("rocksdb iterator error in embeddings CF")?;
        stats.scanned += 1;
        let chunk_id = String::from_utf8_lossy(&key).into_owned();
        let stream = match lookup_chunk_stream(db, &chunk_id)? {
            Some(s) => s,
            None => {
                stats.orphan += 1;
                continue;
            }
        };
        if !cfg.matches(&stream) {
            continue;
        }
        let embedding =
            decode_vec_f32(&value).with_context(|| format!("decode embedding for {chunk_id}"))?;
        let reservoir = by_stream
            .entry(stream.clone())
            .or_insert_with(|| StreamReservoir::new(stream_seed(cfg.seed, &stream), cfg.sample));
        reservoir.offer(chunk_id, embedding);
        stats.matched += 1;
    }
    Ok(stats)
}

fn open_readonly(db_path: &str) -> Result<DB> {
    let opts = Options::default();
    let cf_names = DB::list_cf(&opts, db_path)
        .context("Failed to list column families (DB locked or missing?)")?;
    let cfs: Vec<_> = cf_names
        .iter()
        .map(|name| ColumnFamilyDescriptor::new(name, Options::default()))
        .collect();
    DB::open_cf_descriptors_read_only(&opts, db_path, cfs, false)
        .context("Failed to open RocksDB read-only")
}

fn lookup_chunk_stream(db: &DB, chunk_id: &str) -> Result<Option<String>> {
    for level in 0_i32..=2_i32 {
        let key = format!("chunk:L{level}:{chunk_id}");
        if let Some(bytes) = db
            .get(key.as_bytes())
            .with_context(|| format!("rocksdb get for {key}"))?
        {
            let v: Value = serde_json::from_slice(&bytes)
                .with_context(|| format!("parse chunk JSON {key}"))?;
            return Ok(v.get("stream").and_then(Value::as_str).map(str::to_string));
        }
    }
    Ok(None)
}

/// Decode a `Vec<f32>` serialized by bincode 1.x with default
/// `DefaultOptions`: little-endian `u64` length prefix followed by raw
/// little-endian `f32` bytes. Inline implementation; see module docstring.
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
        let raw_bytes: [u8; 4] = bytes[off..off + 4]
            .try_into()
            .context("slice→[u8;4] for f32")?;
        out.push(f32::from_bits(u32::from_le_bytes(raw_bytes)));
    }
    Ok(out)
}

/// FNV-1a-style mix of base seed and stream id so each stream gets a
/// reproducible but distinct PRNG path. Constants from FNV-1a-64.
fn stream_seed(base: u64, stream: &str) -> u64 {
    const FNV_PRIME: u64 = 0x100_0000_01b3;
    let mut h = base;
    for b in stream.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(FNV_PRIME);
    }
    if h == 0 {
        1
    } else {
        h
    }
}

/// Reservoir-A (Vitter, Algorithm R) sampling. O(capacity) memory per
/// stream regardless of incoming volume.
struct StreamReservoir {
    items: Vec<(String, Vec<f32>)>,
    capacity: usize,
    seen: u64,
    rng: Xorshift64,
}

impl StreamReservoir {
    fn new(seed: u64, capacity: usize) -> Self {
        Self {
            items: Vec::with_capacity(capacity),
            capacity,
            seen: 0,
            rng: Xorshift64::new(seed),
        }
    }

    fn offer(&mut self, id: String, emb: Vec<f32>) {
        if self.capacity == 0 {
            return;
        }
        self.seen += 1;
        if self.items.len() < self.capacity {
            self.items.push((id, emb));
            return;
        }
        let j = usize::try_from(self.rng.next_u64() % self.seen).unwrap_or(0);
        if j < self.capacity {
            self.items[j] = (id, emb);
        }
    }

    fn into_items(self) -> std::vec::IntoIter<(String, Vec<f32>)> {
        self.items.into_iter()
    }
}

struct Xorshift64 {
    state: u64,
}

impl Xorshift64 {
    fn new(seed: u64) -> Self {
        Self {
            state: if seed == 0 { 1 } else { seed },
        }
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        x
    }
}

fn extract_value(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1).cloned())
}

fn parse_usize_flag(args: &[String], flag: &str, default: usize) -> Result<usize> {
    match extract_value(args, flag) {
        None => Ok(default),
        Some(s) => s
            .parse::<usize>()
            .with_context(|| format!("{flag} expects a non-negative integer, got '{s}'")),
    }
}

fn parse_u64_flag(args: &[String], flag: &str, default: u64) -> Result<u64> {
    match extract_value(args, flag) {
        None => Ok(default),
        Some(s) => s
            .parse::<u64>()
            .with_context(|| format!("{flag} expects a non-negative integer, got '{s}'")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_vec_f32_roundtrip_simple() {
        // bincode 1.x default: u64 LE length + raw f32 LE bytes.
        let mut buf: Vec<u8> = Vec::new();
        buf.extend_from_slice(&3u64.to_le_bytes());
        for v in [1.0_f32, -2.5, 3.25] {
            buf.extend_from_slice(&v.to_bits().to_le_bytes());
        }
        let out = decode_vec_f32(&buf).expect("decode");
        assert_eq!(out, vec![1.0_f32, -2.5, 3.25]);
    }

    #[test]
    fn decode_vec_f32_rejects_short_payload() {
        let buf = vec![0u8; 4];
        assert!(decode_vec_f32(&buf).is_err());
    }

    #[test]
    fn decode_vec_f32_rejects_length_mismatch() {
        let mut buf: Vec<u8> = Vec::new();
        buf.extend_from_slice(&5u64.to_le_bytes()); // promises 5
        buf.extend_from_slice(&1.0_f32.to_bits().to_le_bytes()); // delivers 1
        assert!(decode_vec_f32(&buf).is_err());
    }

    #[test]
    fn stream_seed_is_stable_and_distinct() {
        let a = stream_seed(42, "__shared_main__");
        let b = stream_seed(42, "__shared_main__");
        let c = stream_seed(42, "private-x");
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_ne!(stream_seed(0, "x"), 0);
    }

    #[test]
    fn reservoir_capacity_zero_emits_nothing() {
        let mut r = StreamReservoir::new(1, 0);
        r.offer("a".to_string(), vec![0.0]);
        r.offer("b".to_string(), vec![1.0]);
        assert_eq!(r.into_items().count(), 0);
    }

    #[test]
    fn reservoir_below_capacity_keeps_all() {
        let mut r = StreamReservoir::new(1, 10);
        for i in 0..5 {
            r.offer(format!("id-{i}"), vec![i as f32]);
        }
        let items: Vec<_> = r.into_items().collect();
        assert_eq!(items.len(), 5);
    }

    #[test]
    fn reservoir_above_capacity_caps_at_capacity_and_is_deterministic() {
        let mut r1 = StreamReservoir::new(7, 3);
        let mut r2 = StreamReservoir::new(7, 3);
        for i in 0..100 {
            r1.offer(format!("id-{i}"), vec![i as f32]);
            r2.offer(format!("id-{i}"), vec![i as f32]);
        }
        let a: Vec<_> = r1.into_items().map(|(id, _)| id).collect();
        let b: Vec<_> = r2.into_items().map(|(id, _)| id).collect();
        assert_eq!(a.len(), 3);
        assert_eq!(a, b);
    }

    #[test]
    fn cfg_parse_requires_stream_or_all() {
        let args = vec!["bin".to_string(), "--sample".to_string(), "10".to_string()];
        assert!(Cfg::parse(&args).is_err());
    }

    #[test]
    fn cfg_parse_accepts_explicit_stream() {
        let args = vec![
            "bin".to_string(),
            "--stream".to_string(),
            "s1".to_string(),
            "--sample".to_string(),
            "5".to_string(),
            "--seed".to_string(),
            "11".to_string(),
        ];
        let cfg = Cfg::parse(&args).expect("parse");
        assert!(cfg.matches("s1"));
        assert!(!cfg.matches("s2"));
        assert_eq!(cfg.sample, 5);
        assert_eq!(cfg.seed, 11);
    }

    #[test]
    fn cfg_parse_all_streams_matches_anything() {
        let args = vec!["bin".to_string(), "--all-streams".to_string()];
        let cfg = Cfg::parse(&args).expect("parse");
        assert!(cfg.matches("anything"));
        assert!(cfg.matches("__shared_main__"));
    }
}
