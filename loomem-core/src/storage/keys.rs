//! Meta key constants for RocksDB. Shared between loomem-core (server)
//! and loomem-migrate (binary) to prevent rename drift.
//!
//! Cycle /50 — extracted from `loomem-core/src/storage.rs` and
//! `loomem-migrate/src/main.rs` to deduplicate raw byte literals.
//!
//! Note: `loomem-migrate` is intentionally kept self-contained (no loomem-core
//! transitive dep) so it can run against offline snapshots. Local mirrors of
//! these constants in `loomem-migrate/src/main.rs` carry an explicit sync
//! comment pointing here as the canonical source.

/// `meta:schema_version` — schema version stored as utf8 string of u32.
/// Read at server boot to drive schema-bump-triggered Tantivy rebuild.
pub const SCHEMA_VERSION_KEY: &[u8] = b"meta:schema_version";

/// `meta:tantivy_rebuild_needed` — boolean flag (utf8 "1"/"0") set by
/// loomem-migrate after chunk-affecting migrations, read at server boot
/// to trigger explicit Tantivy rebuild. Cycle /49 introduced.
pub const TANTIVY_REBUILD_FLAG_KEY: &[u8] = b"meta:tantivy_rebuild_needed";

/// `meta:encrypt_backfill:progress` — JSON-serialised `BackfillProgress`
/// written after each batch and on completion/error. Absence means the
/// backfill has never been run. Cycle /147 introduced.
pub const ENCRYPT_BACKFILL_PROGRESS_KEY: &[u8] = b"meta:encrypt_backfill:progress";
