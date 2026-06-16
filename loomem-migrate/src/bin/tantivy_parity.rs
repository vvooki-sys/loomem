//! /109 AC-4 — Tantivy parity check.
//!
//! Compares:
//!   (a) Tantivy num_docs() — total indexed docs in the default CF index.
//!   (b) Chunks CF count — L0 + L1 non-tombstoned rows.
//!
//! Also performs a 100-sample cross-check: picks first 100 doc IDs from
//! Tantivy and verifies each has a corresponding chunk in chunks CF.
//!
//! Usage:
//!   tantivy_parity <db_path> <tantivy_index_path>
//!
//! tantivy_index_path is typically <data_dir>/tantivy or similar.
//! Run `find <data_dir> -name 'meta.json' 2>/dev/null` to locate it.

use anyhow::{Context, Result};
use rocksdb::{ColumnFamilyDescriptor, IteratorMode, Options, DB};
use tantivy::schema::Value;
use tantivy::{Index, ReloadPolicy};

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("Usage: tantivy_parity <db_path> <tantivy_index_path>");
        std::process::exit(1);
    }
    let db_path = &args[1];
    let tantivy_path = &args[2];

    eprintln!("tantivy_parity");
    eprintln!("  db_path       : {db_path}");
    eprintln!("  tantivy_path  : {tantivy_path}");

    let db = open_readonly(db_path)?;

    // Count chunks CF (L0 + L1 non-tombstoned)
    let chunks_count = count_chunks_cf(&db)?;
    eprintln!("  chunks CF (L0+L1, non-tombstoned) : {chunks_count}");

    // Open Tantivy index
    let index = Index::open_in_dir(tantivy_path)
        .with_context(|| format!("open tantivy index at {tantivy_path}"))?;
    let reader = index
        .reader_builder()
        .reload_policy(ReloadPolicy::Manual)
        .try_into()
        .context("build tantivy reader")?;
    let searcher = reader.searcher();
    let tantivy_count = searcher.num_docs();
    eprintln!("  tantivy num_docs()                : {tantivy_count}");

    let delta = i64::try_from(tantivy_count).unwrap_or(i64::MAX)
        - i64::try_from(chunks_count).unwrap_or(i64::MAX);
    eprintln!("  delta (tantivy - chunks_cf)       : {delta}");

    // Sample first 100 Tantivy doc IDs from segment readers
    let schema = index.schema();
    let id_field = schema.get_field("id").context("'id' field not in schema")?;

    let mut tantivy_ids: Vec<String> = Vec::new();
    'outer: for seg_reader in searcher.segment_readers() {
        let store = seg_reader.get_store_reader(100)?;
        for doc_id in 0..seg_reader.num_docs() {
            if tantivy_ids.len() >= 100 {
                break 'outer;
            }
            if seg_reader.is_deleted(doc_id) {
                continue;
            }
            let doc: tantivy::TantivyDocument = store.get(doc_id)?;
            if let Some(id_str) = doc.get_first(id_field).and_then(|v| v.as_str()) {
                tantivy_ids.push(id_str.to_string());
            }
        }
    }

    eprintln!(
        "  tantivy sample collected          : {}",
        tantivy_ids.len()
    );

    // Cross-check each Tantivy ID against chunks CF
    let mut miss_count = 0usize;
    for id in &tantivy_ids {
        if !chunk_exists_in_cf(&db, id)? {
            miss_count += 1;
            eprintln!("  TANTIVY_MISS: {id} not found in chunks CF");
        }
    }

    eprintln!(
        "  cross-check misses (100 sample)   : {miss_count}/{}",
        tantivy_ids.len()
    );

    println!("chunks_cf_count={chunks_count}");
    println!("tantivy_num_docs={tantivy_count}");
    println!("delta={delta}");
    println!("sample_size={}", tantivy_ids.len());
    println!("sample_misses={miss_count}");

    Ok(())
}

fn count_chunks_cf(db: &DB) -> Result<u64> {
    let mut count = 0u64;
    for level in 0..=1 {
        let prefix = format!("chunk:L{level}:");
        let iter = db.iterator(IteratorMode::From(
            prefix.as_bytes(),
            rocksdb::Direction::Forward,
        ));
        for item in iter {
            let (key, value) = item.context("rocksdb iter error")?;
            if !key.starts_with(prefix.as_bytes()) {
                break;
            }
            // Skip tombstoned chunks (deleted_at != null)
            let v: serde_json::Value =
                serde_json::from_slice(&value).unwrap_or(serde_json::Value::Null);
            let tombstoned = v.get("deleted_at").and_then(|x| x.as_str()).is_some();
            if !tombstoned {
                count += 1;
            }
        }
    }
    Ok(count)
}

fn chunk_exists_in_cf(db: &DB, chunk_id: &str) -> Result<bool> {
    for level in 0..=2 {
        let key = format!("chunk:L{level}:{chunk_id}");
        if db.get(key.as_bytes())?.is_some() {
            return Ok(true);
        }
    }
    Ok(false)
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
