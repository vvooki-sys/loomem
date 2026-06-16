//! Tantivy rebuild-on-flag helper.
//!
//! Extracted from `loomem-server::main` boot sequence (cycle /49) so the
//! same logic is callable from integration tests without duplicating it.
//!
//! Used by the server boot path (after schema_version check, before drift check)
//! and by the AC-4 integration test.

use anyhow::{Context, Result};
use tracing::info;

use crate::storage::RocksDbStore;
use crate::tantivy_index::TantivyIndex;

/// Check the `meta:tantivy_rebuild_needed` flag and, if set, rebuild Tantivy
/// from RocksDB and clear the flag.
///
/// Returns `true` if a rebuild was triggered, `false` if the flag was unset.
///
/// Used by server boot (cycle /49) and integration tests (AC-4).
pub fn rebuild_tantivy_if_flag_set(
    store: &RocksDbStore,
    tantivy: &mut TantivyIndex,
) -> Result<bool> {
    if !store.get_tantivy_rebuild_needed().unwrap_or(false) {
        return Ok(false);
    }

    info!("meta:tantivy_rebuild_needed=1 detected → triggering rebuild_from_rocksdb at startup");
    tantivy
        .rebuild_from_rocksdb(store)
        .context("Failed to rebuild Tantivy after migrate flag")?;
    store
        .set_tantivy_rebuild_needed(false)
        .context("Failed to clear tantivy_rebuild_needed flag after rebuild")?;
    info!("Tantivy rebuild complete; flag cleared");

    Ok(true)
}
