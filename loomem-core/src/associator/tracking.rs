//! Association effectiveness tracking (ECA-27).
//!
//! Tracks which surfaced associations are consumed (clicked/used)
//! and computes utilization rates per mechanism.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tracing::debug;

use crate::storage::RocksDbStore;

/// Track that a surfaced association was consumed (clicked/used).
pub fn track_association_consumed(
    store: &RocksDbStore,
    chunk_id: &str,
    mechanism: &str,
) -> Result<()> {
    let now = chrono::Utc::now().timestamp() as u64;
    let key = format!("assoc:consumed:{}:{}", chunk_id, now);
    store.put(key.as_bytes(), mechanism.as_bytes())?;
    debug!(
        "Tracked association consumed: chunk={} mechanism={}",
        chunk_id, mechanism
    );
    Ok(())
}

/// Per-mechanism effectiveness statistics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MechanismStats {
    pub by_mechanism: Vec<MechanismEntry>,
    pub total_consumed: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MechanismEntry {
    pub mechanism: String,
    pub consumed_count: usize,
}

/// Get utilization rate per mechanism by scanning consumed records.
pub fn get_mechanism_effectiveness(
    store: &RocksDbStore,
    _stream_id: &str,
) -> Result<MechanismStats> {
    let prefix = b"assoc:consumed:";
    let mut by_mechanism: HashMap<String, usize> = HashMap::new();
    let mut total = 0usize;

    for (_key, value) in store.prefix_scan(prefix) {
        let mechanism = String::from_utf8_lossy(&value).to_string();
        *by_mechanism.entry(mechanism).or_insert(0) += 1;
        total += 1;
    }

    let mut entries: Vec<MechanismEntry> = by_mechanism
        .into_iter()
        .map(|(mechanism, consumed_count)| MechanismEntry {
            mechanism,
            consumed_count,
        })
        .collect();
    entries.sort_by_key(|b| std::cmp::Reverse(b.consumed_count));

    Ok(MechanismStats {
        by_mechanism: entries,
        total_consumed: total,
    })
}
