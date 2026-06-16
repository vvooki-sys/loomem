//! /109 AC-2 classification helper.
//! Quick probe: for each chunk_id passed as arg, checks all possible
//! chunk key patterns to classify as Class A (key absent) or Class C/D.
//!
//! Usage: db_probe <db_path> <chunk_id1> [chunk_id2 ...]

use anyhow::{Context, Result};
use rocksdb::{ColumnFamilyDescriptor, IteratorMode, Options, DB};

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("Usage: db_probe <db_path> <chunk_id1> [chunk_id2 ...]");
        std::process::exit(1);
    }
    let db_path = &args[1];
    let chunk_ids: Vec<&str> = args[2..].iter().map(String::as_str).collect();

    let db = open_readonly(db_path)?;

    for chunk_id in chunk_ids {
        let mut found_key = None;
        let mut deleted_at = None;

        // Standard lookup: L0/L1/L2 with the embedding key as-is
        for level in 0..=2 {
            let key = format!("chunk:L{level}:{chunk_id}");
            if let Some(bytes) = db.get(key.as_bytes())? {
                let v: serde_json::Value =
                    serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
                deleted_at = v
                    .get("deleted_at")
                    .and_then(|x| x.as_str())
                    .map(|s| s.to_string());
                found_key = Some(key);
                break;
            }
        }

        // For L1:-prefixed orphans: strip the "L1:" prefix and try again
        // (In case the actual UUID is stored without the prefix)
        let stripped_id = chunk_id.strip_prefix("L1:");

        let mut found_stripped = None;
        if found_key.is_none() {
            if let Some(sid) = stripped_id {
                for level in 0..=2 {
                    let key = format!("chunk:L{level}:{sid}");
                    if let Some(bytes) = db.get(key.as_bytes())? {
                        let v: serde_json::Value =
                            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
                        let da = v
                            .get("deleted_at")
                            .and_then(|x| x.as_str())
                            .map(|s| s.to_string());
                        found_stripped = Some((key, da));
                        break;
                    }
                }
            }
        }

        // Also check: scan any chunk key that contains this UUID substring (L2-era format)
        // Only for bare-UUID orphans, check if there's a chunk key like chunk:L2:<uuid>
        // (pre-K1 L2 tier). We need to look in all raw keys.
        let uuid_part = stripped_id.unwrap_or(chunk_id);
        let l2_key = format!("chunk:L2:{uuid_part}");
        let l2_exists = db.get(l2_key.as_bytes())?.is_some();

        if let Some(ref k) = found_key {
            let class = if deleted_at.is_some() {
                "CLASS_B (tombstone-active)"
            } else {
                "CLASS_A_FOUND (not orphan?)"
            };
            println!("CHUNK_ID={chunk_id}  FOUND_AT={k}  deleted_at={deleted_at:?}  CLASS={class}");
        } else if let Some((ref k, ref da)) = found_stripped {
            println!("CHUNK_ID={chunk_id}  FOUND_STRIPPED_AT={k}  deleted_at={da:?}  CLASS=CLASS_D (encoding: L1: prefix in embedding key, plain UUID in chunk key)");
        } else if l2_exists {
            println!("CHUNK_ID={chunk_id}  FOUND_L2={l2_key}  CLASS=CLASS_D_L2 (chunk:L2: still exists, embedding key stripped)");
        } else {
            println!("CHUNK_ID={chunk_id}  NOT_FOUND  CLASS=CLASS_A (key absent all levels)");
        }
    }

    // Also count L2 chunk keys remaining in DB (post-K1 should be near 0)
    let mut l2_count = 0u64;
    let prefix = b"chunk:L2:";
    let iter = db.iterator(IteratorMode::From(prefix, rocksdb::Direction::Forward));
    for item in iter {
        let (key, _) = item?;
        if !key.starts_with(prefix) {
            break;
        }
        l2_count += 1;
    }
    eprintln!("L2_CHUNK_COUNT_IN_DB={l2_count}");

    Ok(())
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
