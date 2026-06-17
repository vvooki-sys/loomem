use anyhow::{Context, Result};
use rocksdb::{IteratorMode, Options, DB};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Arc;
use tracing::{debug, info, warn};

use crate::intent_log::IntentLogConfig;
use crate::source_tag::{deserialize_source_compat, SourceTag};
use crate::tantivy_index::TantivyConfig;

pub mod persist;
pub mod scan_log;
pub use persist::{persist_chunk_with_index, PersistChunkArgs};

pub mod keys;
pub use keys::{SCHEMA_VERSION_KEY, TANTIVY_REBUILD_FLAG_KEY};

pub mod rebuild;
pub use rebuild::rebuild_tantivy_if_flag_set;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageConfig {
    pub data_dir: std::path::PathBuf,
    pub rocksdb: RocksDbConfig,
    pub tantivy: TantivyConfig,
    pub vector_enabled: bool,
    #[serde(default)]
    pub intent_log: IntentLogConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RocksDbConfig {
    pub max_open_files: i32,
    pub compression: String,
    pub write_buffer_size: usize,
    pub max_write_buffer_number: i32,
}

const CF_EMBEDDINGS: &str = "embeddings";
const CF_COSTS: &str = "costs";
/// Cycle /134 §B: per-stream wrapped DEK storage for envelope encryption.
/// Rows keyed `scope:{scope}` → bincode-serialized `WrappedStreamDek`.
pub(crate) const CF_KEYS: &str = "keys";

/// System-reserved default stream for single-user deployments.
/// Double underscore convention is reserved for system streams so it cannot
/// collide with user-provisioned `stream_id` values (UUID-shaped).
pub const DEFAULT_STREAM_ID: &str = "__user_default__";

fn default_true() -> bool {
    true
}
fn default_version() -> u32 {
    1
}

/// Cycle/112: Beta(1,1) uniform prior — chunk has no feedback yet.
fn default_alpha() -> f64 {
    1.0
}
fn default_beta() -> f64 {
    1.0
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum FactType {
    PreferenceOrDecision,
    ProjectState,
    Fact,
    Event,
    Experience,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractionMeta {
    pub fact_type: FactType,
    pub subject: Option<String>,
    pub event_date: Option<String>,
    pub event_date_context: Option<String>,
    pub supersedes: Option<String>,
    pub superseded_by: Option<String>,
    pub confidence: f64,
    pub extracted_from: Option<String>,
    pub extraction_model: Option<String>,
    /// /156: the original extracted text before a history-preserving rewrite
    /// merged it with a superseded memory. Audit-only — not indexed. `None`
    /// when no rewrite occurred (serde default keeps old chunks deserializing
    /// unchanged).
    #[serde(default)]
    pub original_content: Option<String>,
    /// Operator-configured custom topic key (from `[knowledge_extraction].topics`)
    /// when the extracted `fact_type` is not one of the built-in `FactType`
    /// variants. Built-in types collapse the raw key into the enum, so this
    /// preserves the configured key (e.g. `risk_item`, `contact`) for filtering.
    /// `None` for built-in types and for chunks written before this field
    /// existed (serde default keeps old chunks deserializing unchanged).
    #[serde(default)]
    pub topic: Option<String>,
}

impl ExtractionMeta {
    /// /151 (port of /114b1) — parse `event_date` (ISO `YYYY-MM-DD`) into a
    /// UTC midnight unix timestamp suitable for `Chunk.valid_from`. Returns
    /// `None` when the field is absent, unparseable, or pre-1970 (negative
    /// unix time cannot be represented in the `u64` `valid_from` field).
    #[must_use]
    pub fn event_date_unix(&self) -> Option<u64> {
        let raw = self.event_date.as_deref()?;
        let ts = chrono::NaiveDate::parse_from_str(raw.trim(), "%Y-%m-%d")
            .ok()?
            .and_hms_opt(0, 0, 0)?
            .and_utc()
            .timestamp();
        u64::try_from(ts).ok()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Chunk {
    pub id: String,
    pub content: String,
    pub stream: String,
    /// Memory tier. Valid values: `0` (raw chunk) or `1` (consolidated).
    /// `level >= 2` was the L2 abstraction tier, removed 2026-05-09 per
    /// cycle/k1-drop-L2-tier. Writes with level >= 2 are guarded by
    /// `debug_assert!` in `store_chunk` and emit a warn log in production.
    /// See `cycles/cycle-k1-drop-L2-tier-close.md` for context.
    pub level: i32,
    pub score: f64,
    pub timestamp: u64,
    pub consolidated: bool,
    pub dormant: bool,
    pub in_progress: bool,
    pub prompt_version: Option<u32>,
    pub source_ids: Option<Vec<String>>,
    pub last_decay: Option<u64>,
    pub metadata: Option<serde_json::Value>,
    #[serde(default)]
    pub importance: Option<f64>,
    #[serde(default)]
    pub persistent: bool,
    #[serde(default)]
    pub last_implicit_boost: Option<u64>,
    #[serde(default)]
    pub access_count: u32,
    #[serde(default, deserialize_with = "deserialize_source_compat")]
    pub source: Option<SourceTag>,
    #[serde(default)]
    pub created_by: Option<String>,
    #[serde(default)]
    pub updated_at: Option<u64>,
    #[serde(default)]
    pub valid_from: Option<u64>,
    #[serde(default)]
    pub valid_until: Option<u64>,
    // v2: contradiction handling + profile layer
    #[serde(default = "default_true")]
    pub is_latest: bool,
    #[serde(default)]
    pub superseded_by: Option<String>,
    #[serde(default)]
    pub supersedes_id: Option<String>,
    #[serde(default)]
    pub root_memory_id: Option<String>,
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default)]
    pub memory_type: Option<String>,
    #[serde(default)]
    pub extraction_meta: Option<ExtractionMeta>,
    // v3: soft delete + trust tiers
    #[serde(default)]
    pub deleted_at: Option<u64>,
    /// Trust level: "a1" (full trust), "a2" (derived/assistant), "b" (external/untrusted).
    /// None = "a1" for backward compat.
    #[serde(default)]
    pub trust_level: Option<String>,
    /// User ID of the authenticated user who ingested this chunk.
    /// None for legacy chunks written before Cycle 13 Faza 2.
    #[serde(default)]
    pub ingester_user_id: Option<String>,
    /// Cycle/112: Bayesian utility tally (Beta posterior over usefulness).
    /// Default Beta(1,1) = uniform prior — chunk has no feedback yet.
    #[serde(default = "default_alpha")]
    pub alpha: f64,
    #[serde(default = "default_beta")]
    pub beta: f64,

    /// Cycle/112: how many times this chunk has been flagged as actively harmful.
    #[serde(default)]
    pub harmful_count: u32,

    /// Cycle/112: total number of rating events for this chunk.
    #[serde(default)]
    pub n_ratings: u32,

    /// Cycle/112: unix ms timestamp of most recent rating event. None = never rated.
    #[serde(default)]
    pub last_rated_at: Option<i64>,
}

/// /138 phase 2: storage-layer envelope for a chunk written with field-level
/// encryption. Wraps the in-memory `Chunk` (unchanged) and carries the
/// `encrypted_payload` next to the plaintext routing fields. Serialize-only and
/// used solely on the encrypted write path; readers still deserialize the raw
/// value as `Chunk`, ignoring the extra `encrypted_payload` key until §D wires
/// decrypt-on-read. Defined as a wrapper (not a `Chunk` field) so the ~20
/// `Chunk { .. }` construction sites across the workspace stay untouched.
/// See ADR-013 §4 (post-/138-phase1 amendment).
#[derive(Serialize)]
struct StoredChunk<'a> {
    #[serde(flatten)]
    chunk: &'a Chunk,
    /// AES-256-GCM blob of the serde_json-encoded tuple
    /// `(content, metadata, extraction_meta)` under the chunk's `stream` DEK.
    /// Tuple order is the wire format §D (decrypt) and §F (backfill) must match.
    encrypted_payload: Vec<u8>,
}

/// /138 §D: read-side inverse of `StoredChunk<'a>`. Deserializes the on-disk
/// envelope: the flattened plaintext routing fields land in `chunk`, and the
/// optional `encrypted_payload` (absent ⇒ empty for legacy/NoopProvider
/// plaintext rows) carries the field-level ciphertext. `decode_chunk` is the
/// single chokepoint that reconstitutes a `Chunk` from raw bytes; every chunk
/// read routes through it so the corruption-prone read-modify-`store_chunk`
/// loops never re-encrypt cleared content.
#[derive(Deserialize)]
pub(crate) struct StoredChunkRead {
    #[serde(flatten)]
    pub(crate) chunk: Chunk,
    #[serde(default)]
    pub(crate) encrypted_payload: Vec<u8>,
}

/// Derive trust level from source field.
pub fn derive_trust_level(source: Option<&str>) -> String {
    match source {
        Some("api")
        | Some("user_direct")
        | Some("confirmed_preference")
        | Some("first_party_doc") => "a1",
        Some("mcp") | Some("mcp-ingest") | Some("assistant_generated") => "a2",
        Some("external_web") | Some("imported_third_party") | Some("firecrawl") | Some("ocr") => {
            "b"
        }
        None => "a1",   // legacy data = user-generated
        Some(_) => "b", // unknown = untrusted
    }
    .to_string()
}

/// Emit a warn-level log if `chunk.level > 1` (zombie L2 write guard).
/// Extracted to allow unit-testing warn-emission without needing `debug_assert!`
/// to pass. Called from `store_chunk` after the `debug_assert!`.
fn warn_if_zombie_level(chunk: &Chunk) {
    if chunk.level > 1 {
        tracing::warn!(
            chunk_id = %chunk.id,
            level = chunk.level,
            "L2 zombie write detected (tier removed 2026-05-09); writing anyway for forward-compat"
        );
    }
}

pub struct RocksDbStore {
    db: Arc<DB>,
    /// Cycle /134 §B: encryption-at-rest provider. Held but NOT yet wired into
    /// read/write paths in §B (that is §C/§D). Defaults to `NoopProvider`.
    encryption: Arc<dyn crate::crypto::provider::EncryptionProvider>,
    /// /157 S3: decode-failure summary of the most recent full chunk scan
    /// (`get_all_chunks`) — source of the `undecodable_chunks` status counter.
    last_scan_decode: std::sync::Mutex<Option<scan_log::ScanDecodeSummary>>,
    /// /159 S2: snapshot of `LOOMEM_AT_REST_MASTER_KEY` presence taken at
    /// `open()`. When the process is configured for encryption at rest but
    /// the provider is still the default `NoopProvider`, a chunk scan is a
    /// pre-attach scan — its decode failures are encrypted rows read without
    /// a key, not corruption, and must not feed the status counter.
    at_rest_key_present: bool,
}

impl RocksDbStore {
    /// Get a reference to the underlying DB
    pub fn db(&self) -> &DB {
        &self.db
    }

    /// Cycle /134 §B: clone of the shared DB handle, for injecting into an
    /// `EncryptionProvider` that must read/write the `keys` column family.
    /// Sharing the same `Arc<DB>` avoids a second (impossible) RocksDB open.
    pub fn db_arc(&self) -> Arc<DB> {
        Arc::clone(&self.db)
    }

    /// Cycle /134 §B: the held encryption provider.
    pub fn encryption_provider(&self) -> &Arc<dyn crate::crypto::provider::EncryptionProvider> {
        &self.encryption
    }

    /// Cycle /134 §B: builder injecting an encryption provider. Used instead of
    /// a `new()` signature change because `open()` has ~80 call sites; this keeps
    /// them all defaulting to `NoopProvider` (brief §B Open Question #2).
    #[must_use]
    pub fn with_encryption_provider(
        mut self,
        provider: Arc<dyn crate::crypto::provider::EncryptionProvider>,
    ) -> Self {
        self.encryption = provider;
        self
    }

    pub fn open<P: AsRef<Path>>(path: P, config: &RocksDbConfig) -> Result<Self> {
        let path = path.as_ref();
        info!("Opening RocksDB at: {}", path.display());

        let mut opts = Options::default();
        opts.create_if_missing(true);
        opts.create_missing_column_families(true);
        opts.set_max_open_files(config.max_open_files);
        opts.set_write_buffer_size(config.write_buffer_size);
        opts.set_max_write_buffer_number(config.max_write_buffer_number);

        // Set compression
        match config.compression.as_str() {
            "none" => opts.set_compression_type(rocksdb::DBCompressionType::None),
            "snappy" => opts.set_compression_type(rocksdb::DBCompressionType::Snappy),
            "zlib" => opts.set_compression_type(rocksdb::DBCompressionType::Zlib),
            "lz4" => opts.set_compression_type(rocksdb::DBCompressionType::Lz4),
            "zstd" => opts.set_compression_type(rocksdb::DBCompressionType::Zstd),
            _ => {
                tracing::warn!(
                    "Unknown compression type: {}, using Snappy",
                    config.compression
                );
                opts.set_compression_type(rocksdb::DBCompressionType::Snappy);
            }
        }

        // Open with column families. Cycle /134 §B adds `keys`;
        // `create_missing_column_families(true)` above ensures fresh databases
        // and older databases both pick them up without explicit migration.
        let db = DB::open_cf(&opts, path, [CF_EMBEDDINGS, CF_COSTS, CF_KEYS])
            .with_context(|| format!("Failed to open RocksDB at {}", path.display()))?;

        info!("RocksDB opened successfully with embeddings, costs, and keys CFs");
        Ok(Self {
            db: Arc::new(db),
            encryption: Arc::new(crate::crypto::provider::NoopProvider),
            last_scan_decode: std::sync::Mutex::new(None),
            at_rest_key_present: std::env::var(crate::crypto::provider::MASTER_KEY_ENV).is_ok(),
        })
    }

    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        debug!("RocksDB get: key_len={}", key.len());
        self.db.get(key).context("Failed to get value from RocksDB")
    }

    pub fn put(&self, key: &[u8], value: &[u8]) -> Result<()> {
        debug!(
            "RocksDB put: key_len={}, value_len={}",
            key.len(),
            value.len()
        );
        self.db
            .put(key, value)
            .context("Failed to put value into RocksDB")
    }

    pub fn delete(&self, key: &[u8]) -> Result<()> {
        debug!("RocksDB delete: key_len={}", key.len());
        self.db
            .delete(key)
            .context("Failed to delete key from RocksDB")
    }

    pub fn scan(&self) -> impl Iterator<Item = (Box<[u8]>, Box<[u8]>)> + '_ {
        debug!("RocksDB scan: full database");
        self.db
            .iterator(IteratorMode::Start)
            .filter_map(|result| match result {
                Ok(v) => Some(v),
                Err(e) => {
                    tracing::error!("RocksDB iterator error: {}", e);
                    None
                }
            })
    }

    pub fn prefix_scan<'a>(
        &'a self,
        prefix: &'a [u8],
    ) -> impl Iterator<Item = (Box<[u8]>, Box<[u8]>)> + 'a {
        debug!("RocksDB prefix_scan: prefix_len={}", prefix.len());
        self.db
            .iterator(IteratorMode::From(prefix, rocksdb::Direction::Forward))
            .take_while(move |result| {
                if let Ok((key, _)) = result {
                    key.starts_with(prefix)
                } else {
                    false
                }
            })
            .filter_map(|result| match result {
                Ok(v) => Some(v),
                Err(e) => {
                    tracing::error!("RocksDB iterator error: {}", e);
                    None
                }
            })
    }

    pub fn compact(&self) -> Result<()> {
        info!("Running RocksDB compaction");
        self.db.compact_range::<&[u8], &[u8]>(None, None);
        Ok(())
    }

    pub fn create_checkpoint(&self, dest: &Path) -> Result<()> {
        let checkpoint = rocksdb::checkpoint::Checkpoint::new(&self.db)
            .context("Failed to create RocksDB checkpoint object")?;
        checkpoint
            .create_checkpoint(dest)
            .with_context(|| format!("Failed to create checkpoint at {}", dest.display()))?;
        info!("RocksDB checkpoint created at {}", dest.display());
        Ok(())
    }

    pub fn estimate_num_keys(&self) -> Result<u64> {
        let count = self
            .db
            .property_int_value("rocksdb.estimate-num-keys")
            .context("Failed to get estimated key count")?
            .unwrap_or(0);
        Ok(count)
    }

    /// Store an embedding vector for a document
    pub fn store_embedding(&self, id: &str, vector: Vec<f32>) -> Result<()> {
        let cf = self
            .db
            .cf_handle(CF_EMBEDDINGS)
            .context("Embeddings column family not found")?;

        let encoded =
            bincode::serialize(&vector).context("Failed to serialize embedding vector")?;

        self.db
            .put_cf(&cf, id.as_bytes(), &encoded)
            .context("Failed to store embedding")?;

        debug!("Stored embedding for id={}, dim={}", id, vector.len());
        Ok(())
    }

    /// Delete an embedding vector.
    pub fn delete_embedding(&self, id: &str) -> Result<()> {
        let cf = self
            .db
            .cf_handle(CF_EMBEDDINGS)
            .context("Embeddings column family not found")?;
        self.db
            .delete_cf(&cf, id.as_bytes())
            .context("Failed to delete embedding")?;
        Ok(())
    }

    /// Get an embedding vector for a specific document
    pub fn get_embedding(&self, id: &str) -> Result<Option<Vec<f32>>> {
        let cf = self
            .db
            .cf_handle(CF_EMBEDDINGS)
            .context("Embeddings column family not found")?;

        let result = self
            .db
            .get_cf(&cf, id.as_bytes())
            .context("Failed to get embedding")?;

        match result {
            Some(bytes) => {
                let vector: Vec<f32> = bincode::deserialize(&bytes)
                    .context("Failed to deserialize embedding vector")?;
                Ok(Some(vector))
            }
            None => Ok(None),
        }
    }

    /// Get all embeddings from the store
    pub fn get_all_embeddings(&self) -> Result<Vec<(String, Vec<f32>)>> {
        let cf = self
            .db
            .cf_handle(CF_EMBEDDINGS)
            .context("Embeddings column family not found")?;

        let mut embeddings = Vec::new();

        let iter = self.db.iterator_cf(&cf, IteratorMode::Start);
        for item in iter {
            match item {
                Ok((key, value)) => {
                    let id = String::from_utf8_lossy(&key).to_string();
                    match bincode::deserialize::<Vec<f32>>(&value) {
                        Ok(vector) => embeddings.push((id, vector)),
                        Err(e) => {
                            tracing::error!("Failed to deserialize embedding for {}: {}", id, e);
                        }
                    }
                }
                Err(e) => {
                    tracing::error!("RocksDB iterator error in embeddings CF: {}", e);
                }
            }
        }

        debug!("Retrieved {} embeddings", embeddings.len());
        Ok(embeddings)
    }

    /// Get count of embeddings in the store
    pub fn count_embeddings(&self) -> Result<usize> {
        let cf = self
            .db
            .cf_handle(CF_EMBEDDINGS)
            .context("Embeddings column family not found")?;

        let count = self.db.iterator_cf(&cf, IteratorMode::Start).count();

        Ok(count)
    }

    // ── Cycle /010 S4: embedding-dimension guard ────────────────────────────
    //
    // Changing the embedding provider/model changes the vector dimension
    // (e.g. OpenAI 1536 → local multilingual-e5-small 384). Mixing dimensions
    // in one index silently corrupts hybrid search. We record the dimension a
    // database was built with and refuse to start when the configured
    // dimension disagrees, pointing the operator at re-embedding.

    /// Embedding dimension recorded for this database, if any. Stored in the
    /// default CF under `meta:embedding_dim`.
    pub fn stored_embedding_dim(&self) -> Result<Option<usize>> {
        match self
            .db
            .get(b"meta:embedding_dim")
            .context("read meta:embedding_dim")?
        {
            Some(bytes) => Ok(String::from_utf8_lossy(&bytes).trim().parse::<usize>().ok()),
            None => Ok(None),
        }
    }

    /// Record the embedding dimension for this database (idempotent).
    pub fn set_embedding_dim(&self, dim: usize) -> Result<()> {
        self.db
            .put(b"meta:embedding_dim", dim.to_string().as_bytes())
            .context("write meta:embedding_dim")
    }

    /// Dimension inferred from an actually-stored embedding vector, if any
    /// exist. Lets us validate databases written before the dim meta key
    /// existed (no stored meta, but real 1536-dim vectors present).
    pub fn sampled_embedding_dim(&self) -> Result<Option<usize>> {
        let cf = self
            .db
            .cf_handle(CF_EMBEDDINGS)
            .context("Embeddings column family not found")?;
        if let Some(item) = self.db.iterator_cf(&cf, IteratorMode::Start).next() {
            let (_k, value) = item.context("iterate embeddings for dim sample")?;
            let v: Vec<f32> = bincode::deserialize(&value).context("decode sampled embedding")?;
            return Ok(Some(v.len()));
        }
        Ok(None)
    }

    /// Validate the configured embedding dimension against what this database
    /// was built with, then record it. Errors (refuse to start) on a
    /// mismatch instead of silently mixing vector sizes. A fresh database or
    /// one whose dimension already matches passes and has its dimension
    /// recorded for future starts.
    pub fn validate_and_record_embedding_dim(&self, configured_dim: usize) -> Result<()> {
        let existing = match self.stored_embedding_dim()? {
            Some(d) => Some(d),
            None => self.sampled_embedding_dim()?,
        };
        if let Some(existing) = existing {
            anyhow::ensure!(
                existing == configured_dim,
                "embedding dimension mismatch: this database was built with {existing}-dim \
                 vectors but the configuration specifies {configured_dim}. Re-embed with \
                 `loomem-server --reembed` or restore the previous embedding_provider / \
                 embedding_model / embedding_dim."
            );
        }
        self.set_embedding_dim(configured_dim)?;
        Ok(())
    }

    /// /138 §D: single decode chokepoint, inverse of the `store_chunk` write
    /// envelope. Reconstitutes a `Chunk` from raw on-disk bytes: parses the
    /// flattened routing fields, and — if `encrypted_payload` is present —
    /// decrypts `(content, metadata, extraction_meta)` under the chunk's own
    /// `stream` DEK and repopulates the cleared plaintext fields. Legacy
    /// (pre-/138 plaintext) rows have no `encrypted_payload` and return as-is.
    /// Every chunk read in the workspace routes through here. Public so the
    /// `loomem-server` crate's prefix-scan read paths (dashboard, context) can
    /// reconstitute chunks without bypassing decryption.
    pub fn decode_chunk(&self, bytes: &[u8]) -> Result<Chunk> {
        let staged: StoredChunkRead =
            serde_json::from_slice(bytes).context("Failed to deserialize chunk envelope")?;
        let mut chunk = staged.chunk;
        if staged.encrypted_payload.is_empty() {
            // Legacy fall-through: pre-/138 chunk had plaintext content populated
            // directly. Removed post-/F backfill confirmed (separate cycle TBD).
            return Ok(chunk);
        }
        let payload = self
            .encryption
            .decrypt(&chunk.stream, &staged.encrypted_payload)
            .context("Failed to decrypt chunk payload")?;
        let (content, metadata, extraction_meta): (
            String,
            Option<serde_json::Value>,
            Option<ExtractionMeta>,
        ) = serde_json::from_slice(&payload).context("Failed to deserialize chunk payload")?;
        chunk.content = content;
        chunk.metadata = metadata;
        chunk.extraction_meta = extraction_meta;
        Ok(chunk)
    }

    /// Write-side inverse of `decode_chunk`: encode a chunk into its on-disk
    /// envelope bytes. Single encode chokepoint shared by `store_chunk` and
    /// batched writers (feedback `WriteBatch`) — any write path serializing a
    /// `Chunk` with raw `serde_json::to_vec` instead of this rewrites an
    /// encrypted row back to legacy plaintext (/157 finding 1).
    ///
    /// /138 phase 2: field-level encryption (replaces /134 §C whole-blob).
    /// The content-bearing fields (content, metadata, extraction_meta) are
    /// serialized as a serde_json tuple, encrypted under the chunk's stream
    /// DEK, and stored in `encrypted_payload`; their plaintext copies are
    /// cleared. The routing envelope (id, stream, level, timestamps, flags,
    /// ...) stays plaintext so readers resolve scope + filter without
    /// decrypting. NoopProvider (encryption disabled) keeps the pre-/138
    /// plaintext layout byte-identical. serde_json (not bincode) is used for
    /// the payload because metadata is `serde_json::Value`, which bincode
    /// cannot deserialize (non-self-describing). See ADR-013 §4.
    ///
    /// The zombie-level guards live here (not in `store_chunk`) so every
    /// write-path caller — including the feedback `WriteBatch` — gets the L2
    /// assertion + warn, keeping the chokepoint complete (/157 finding 1).
    pub(crate) fn encode_chunk(&self, chunk: &Chunk) -> Result<Vec<u8>> {
        debug_assert!(
            chunk.level <= 1,
            "L2 tier removed 2026-05-09; chunk.level must be 0 (raw) or 1 (consolidated), got {}",
            chunk.level
        );
        warn_if_zombie_level(chunk);
        if self.encryption.is_enabled() {
            let payload =
                serde_json::to_vec(&(&chunk.content, &chunk.metadata, &chunk.extraction_meta))
                    .context("Failed to serialize chunk payload")?;
            let encrypted = self
                .encryption
                .encrypt(&chunk.stream, &payload)
                .context("Failed to encrypt chunk payload")?;
            let mut cleared = chunk.clone();
            cleared.content = String::new();
            cleared.metadata = None;
            cleared.extraction_meta = None;
            serde_json::to_vec(&StoredChunk {
                chunk: &cleared,
                encrypted_payload: encrypted,
            })
            .context("Failed to serialize chunk envelope")
        } else {
            serde_json::to_vec(chunk).context("Failed to serialize chunk")
        }
    }

    /// Store a chunk in RocksDB. Zombie-level guards run inside `encode_chunk`.
    pub fn store_chunk(&self, chunk: &Chunk) -> Result<()> {
        let key = format!("chunk:L{}:{}", chunk.level, chunk.id);
        let value = self.encode_chunk(chunk)?;

        self.db
            .put(key.as_bytes(), &value)
            .context("Failed to store chunk")?;

        debug!("Stored chunk: {}", chunk.id);
        Ok(())
    }

    /// Get a chunk from RocksDB
    pub fn get_chunk(&self, id: &str) -> Result<Option<Chunk>> {
        // Try all levels
        for level in 0..=2 {
            let key = format!("chunk:L{}:{}", level, id);
            if let Some(bytes) = self.db.get(key.as_bytes())? {
                let chunk = self.decode_chunk(&bytes)?;
                return Ok(Some(chunk));
            }
        }
        Ok(None)
    }

    /// Get a chunk only if it belongs to the given stream. Defense-in-depth.
    pub fn get_chunk_scoped(&self, id: &str, stream: &str) -> Result<Option<Chunk>> {
        match self.get_chunk(id)? {
            Some(chunk) if chunk.stream == stream => Ok(Some(chunk)),
            Some(_) => Ok(None), // exists but wrong stream = invisible
            None => Ok(None),
        }
    }

    /// Check if an ID corresponds to a chunk:* key (vs event:*)
    pub fn is_chunk(&self, id: &str) -> bool {
        for level in 0..=2 {
            let key = format!("chunk:L{}:{}", level, id);
            if let Ok(Some(_)) = self.db.get(key.as_bytes()) {
                return true;
            }
        }
        false
    }

    /// Scan L0 chunks that are unconsolidated and old enough
    pub fn scan_l0_unconsolidated(&self, min_age_secs: u64, limit: usize) -> Result<Vec<Chunk>> {
        let prefix = b"chunk:L0:";
        let mut chunks = Vec::new();

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        for (_, value) in self.prefix_scan(prefix) {
            if chunks.len() >= limit {
                break;
            }

            match self.decode_chunk(&value) {
                Ok(chunk) => {
                    if !chunk.consolidated
                        && !chunk.in_progress
                        && !chunk.dormant
                        && chunk.deleted_at.is_none()
                        && (now - chunk.timestamp) >= min_age_secs
                    {
                        chunks.push(chunk);
                    }
                }
                Err(e) => {
                    warn!("Failed to deserialize L0 chunk: {}", e);
                }
            }
        }

        debug!("Scanned {} unconsolidated L0 chunks", chunks.len());
        Ok(chunks)
    }

    /// Mark chunks as consolidated
    pub fn mark_consolidated(&self, ids: &[String]) -> Result<()> {
        for id in ids {
            if let Some(mut chunk) = self.get_chunk(id)? {
                chunk.consolidated = true;
                chunk.in_progress = false;
                self.store_chunk(&chunk)?;
            }
        }
        Ok(())
    }

    /// Mark chunks as in_progress
    pub fn mark_in_progress(&self, ids: &[String]) -> Result<()> {
        for id in ids {
            if let Some(mut chunk) = self.get_chunk(id)? {
                chunk.in_progress = true;
                self.store_chunk(&chunk)?;
            }
        }
        Ok(())
    }

    /// Clear in_progress flag
    pub fn clear_in_progress(&self, ids: &[String]) -> Result<()> {
        for id in ids {
            if let Some(mut chunk) = self.get_chunk(id)? {
                chunk.in_progress = false;
                self.store_chunk(&chunk)?;
            }
        }
        Ok(())
    }

    /// Scan for chunks eligible for decay (score > min_score, not dormant)
    pub fn scan_for_decay(&self, min_score: f64) -> Result<Vec<Chunk>> {
        let mut chunks = Vec::new();
        let mut scan = scan_log::ScanDecodeLog::new("scan_for_decay");

        for prefix in &[b"chunk:L0:", b"chunk:L1:"] {
            for (key, value) in self.prefix_scan(*prefix) {
                scan.saw_row();
                match self.decode_chunk(&value) {
                    Ok(chunk) => {
                        if !chunk.dormant && chunk.deleted_at.is_none() && chunk.score > min_score {
                            chunks.push(chunk);
                        }
                    }
                    Err(e) => scan.record(&key, &e),
                }
            }
        }
        let _ = scan.finish();

        debug!("Found {} chunks eligible for decay", chunks.len());
        Ok(chunks)
    }

    /// Update chunk score and last_decay timestamp
    pub fn update_score(&self, id: &str, new_score: f64, timestamp: u64) -> Result<()> {
        if let Some(mut chunk) = self.get_chunk(id)? {
            chunk.score = new_score;
            chunk.last_decay = Some(timestamp);
            self.store_chunk(&chunk)?;
        }
        Ok(())
    }

    /// Mark chunk as dormant
    pub fn mark_dormant(&self, id: &str) -> Result<()> {
        if let Some(mut chunk) = self.get_chunk(id)? {
            chunk.dormant = true;
            self.store_chunk(&chunk)?;
        }
        Ok(())
    }

    /// Mark chunk as persistent — exempts from time-based decay.
    /// Idempotent: calling on already-persistent chunk is no-op.
    pub fn mark_persistent(&self, id: &str) -> Result<()> {
        if let Some(mut chunk) = self.get_chunk(id)? {
            if chunk.persistent {
                return Ok(()); // idempotent
            }
            chunk.persistent = true;
            self.store_chunk(&chunk)?;
        }
        Ok(())
    }

    /// Boost chunk score to 1.0 (access boost)
    pub fn boost_score(&self, id: &str) -> Result<()> {
        if let Some(mut chunk) = self.get_chunk(id)? {
            chunk.score = 1.0;
            self.store_chunk(&chunk)?;
            debug!("Boosted score for chunk: {}", id);
        }
        Ok(())
    }

    /// Set importance to high (1.5) and reset score to 1.0
    pub fn boost_importance(&self, id: &str) -> Result<()> {
        if let Some(mut chunk) = self.get_chunk(id)? {
            chunk.importance = Some(1.5);
            chunk.score = 1.0;
            self.store_chunk(&chunk)?;
            debug!("Boosted importance for chunk: {} to 1.5", id);
        }
        Ok(())
    }

    /// Increment access_count for a chunk (called on retrieval for adaptive decay)
    pub fn increment_access_count(&self, id: &str) -> Result<()> {
        if let Some(mut chunk) = self.get_chunk(id)? {
            chunk.access_count = chunk.access_count.saturating_add(1);
            self.store_chunk(&chunk)?;
        }
        Ok(())
    }

    /// Implicit boost: increment importance by delta (capped at max), max once per cooldown_secs
    /// Returns true if boost was applied, false if skipped (cooldown or missing chunk)
    pub fn implicit_boost(
        &self,
        id: &str,
        delta: f64,
        max_importance: f64,
        cooldown_secs: u64,
    ) -> Result<bool> {
        if let Some(mut chunk) = self.get_chunk(id)? {
            let now = chrono::Utc::now().timestamp() as u64;

            // Check cooldown (max 1 boost per cooldown period per chunk)
            if let Some(last) = chunk.last_implicit_boost {
                if now - last < cooldown_secs {
                    debug!(
                        "Implicit boost skipped for {} (cooldown, last={}s ago)",
                        id,
                        now - last
                    );
                    return Ok(false);
                }
            }

            let current = chunk.importance.unwrap_or(1.0);
            let new_importance = (current + delta).min(max_importance);
            chunk.importance = Some(new_importance);
            chunk.last_implicit_boost = Some(now);
            self.store_chunk(&chunk)?;
            debug!(
                "Implicit boost for {}: {:.2} → {:.2}",
                id, current, new_importance
            );
            Ok(true)
        } else {
            Ok(false)
        }
    }

    pub fn reset_all_importance(&self) -> Result<u32> {
        let mut count = 0u32;
        for prefix in &[b"chunk:L0:" as &[u8], b"chunk:L1:"] {
            for (_, value) in self.prefix_scan(prefix) {
                if let Ok(mut chunk) = self.decode_chunk(&value) {
                    if chunk.importance.is_some() || chunk.last_implicit_boost.is_some() {
                        chunk.importance = None;
                        chunk.last_implicit_boost = None;
                        // /138 phase 2: write back through store_chunk so the
                        // field-level envelope is applied (NoopProvider =
                        // byte-identical to pre-/138). Recomputes the same key.
                        self.store_chunk(&chunk)?;
                        count += 1;
                    }
                }
            }
        }
        info!("Reset importance on {} chunks", count);
        Ok(count)
    }

    /// Recover orphaned chunks with in_progress=true
    pub fn recover_orphaned_chunks(&self) -> Result<usize> {
        let mut count = 0;
        let mut scan = scan_log::ScanDecodeLog::new("recover_orphaned_chunks");

        for prefix in &[b"chunk:L0:", b"chunk:L1:"] {
            for (key, value) in self.prefix_scan(*prefix) {
                scan.saw_row();
                match self.decode_chunk(&value) {
                    Ok(mut chunk) => {
                        if chunk.deleted_at.is_some() {
                            continue;
                        }
                        if chunk.in_progress {
                            chunk.in_progress = false;
                            self.store_chunk(&chunk)?;
                            count += 1;
                        }
                    }
                    Err(e) => scan.record(&key, &e),
                }
            }
        }
        let _ = scan.finish();

        Ok(count)
    }

    /// Store entities for a chunk (format: "Name:Type,Name:Type,...")
    ///
    /// Cycle /134 §C: `scope` is the chunk's stream id, used to resolve the
    /// at-rest DEK for the encrypted `entity:*` row (NoopProvider = pass-through).
    pub fn store_entities(
        &self,
        chunk_id: &str,
        scope: &str,
        entities: &[(String, String)],
    ) -> Result<()> {
        let key = format!("entity:{}", chunk_id);
        let value = entities
            .iter()
            .map(|(name, etype)| format!("{}:{}", name, etype))
            .collect::<Vec<_>>()
            .join(",");
        let value = self
            .encryption
            .encrypt(scope, value.as_bytes())
            .context("Failed to encrypt entities")?;
        self.db
            .put(key.as_bytes(), &value)
            .context("Failed to store entities")?;
        debug!("Stored {} entities for chunk: {}", entities.len(), chunk_id);
        Ok(())
    }

    /// Get entities for a chunk (returns name:type pairs or just names for legacy format).
    ///
    /// /138 §D: `scope` is the owning chunk's stream id, used to resolve the
    /// at-rest DEK for the encrypted `entity:*` whole-blob (mirrors the §C
    /// `store_entities` write scope). Callers hold the decrypted chunk, so they
    /// pass `chunk.stream`.
    pub fn get_entities(&self, chunk_id: &str, scope: &str) -> Result<Vec<String>> {
        let key = format!("entity:{}", chunk_id);
        match self.db.get(key.as_bytes())? {
            Some(bytes) => {
                let plaintext = if crate::crypto::at_rest::is_encrypted(&bytes) {
                    // Reject encrypted blob under a disabled provider (downgrade:
                    // master key removed). NoopProvider.decrypt is a pass-through
                    // that would yield ciphertext, which from_utf8_lossy silently
                    // turns into garbage names. Mirror decode_chunk: propagate.
                    anyhow::ensure!(
                        self.encryption.is_enabled(),
                        "entity:{} is encrypted but encryption is disabled (missing master key?)",
                        chunk_id
                    );
                    self.encryption
                        .decrypt(scope, &bytes)
                        .context("Failed to decrypt entities")?
                } else {
                    // Legacy fall-through: pre-/134 §C plaintext. Removed post-/F backfill (TBD).
                    bytes
                };
                let entities_str = String::from_utf8_lossy(&plaintext);
                if entities_str.is_empty() {
                    Ok(Vec::new())
                } else {
                    // Return just the names (strip type suffix if present)
                    Ok(entities_str
                        .split(',')
                        .map(|s| {
                            if let Some(idx) = s.rfind(':') {
                                let potential_type = &s[idx + 1..];
                                if matches!(
                                    potential_type,
                                    "Person" | "Organization" | "Project" | "Technology" | "Place"
                                ) {
                                    s[..idx].to_string()
                                } else {
                                    s.to_string()
                                }
                            } else {
                                s.to_string()
                            }
                        })
                        .collect())
                }
            }
            None => Ok(Vec::new()),
        }
    }

    /// Store relations for a chunk (format: "subj|rel|obj,subj|rel|obj,...")
    ///
    /// Cycle /134 §C: `scope` is the chunk's stream id, used to resolve the
    /// at-rest DEK for the encrypted `rel:*` row (NoopProvider = pass-through).
    pub fn store_relations(
        &self,
        chunk_id: &str,
        scope: &str,
        relations: &[(String, String, String)],
    ) -> Result<()> {
        let key = format!("rel:{}", chunk_id);
        let value = relations
            .iter()
            .map(|(s, r, o)| format!("{}|{}|{}", s, r, o))
            .collect::<Vec<_>>()
            .join(",");
        let value = self
            .encryption
            .encrypt(scope, value.as_bytes())
            .context("Failed to encrypt relations")?;
        self.db
            .put(key.as_bytes(), &value)
            .context("Failed to store relations")?;
        debug!(
            "Stored {} relations for chunk: {}",
            relations.len(),
            chunk_id
        );
        Ok(())
    }

    /// Get relations for a chunk.
    ///
    /// /138 §D: `scope` is the owning chunk's stream id (mirrors the §C
    /// `store_relations` write scope); callers pass `chunk.stream`.
    pub fn get_relations(
        &self,
        chunk_id: &str,
        scope: &str,
    ) -> Result<Vec<(String, String, String)>> {
        let key = format!("rel:{}", chunk_id);
        match self.db.get(key.as_bytes())? {
            Some(bytes) => {
                let plaintext = if crate::crypto::at_rest::is_encrypted(&bytes) {
                    // See get_entities: reject encrypted blob under a disabled
                    // provider (downgrade) rather than emitting garbage relations.
                    anyhow::ensure!(
                        self.encryption.is_enabled(),
                        "rel:{} is encrypted but encryption is disabled (missing master key?)",
                        chunk_id
                    );
                    self.encryption
                        .decrypt(scope, &bytes)
                        .context("Failed to decrypt relations")?
                } else {
                    // Legacy fall-through: pre-/134 §C plaintext. Removed post-/F backfill (TBD).
                    bytes
                };
                let rels_str = String::from_utf8_lossy(&plaintext);
                if rels_str.is_empty() {
                    Ok(Vec::new())
                } else {
                    Ok(rels_str
                        .split(',')
                        .filter_map(|entry| {
                            let parts: Vec<&str> = entry.splitn(3, '|').collect();
                            if parts.len() == 3 {
                                Some((
                                    parts[0].to_string(),
                                    parts[1].to_string(),
                                    parts[2].to_string(),
                                ))
                            } else {
                                None
                            }
                        })
                        .collect())
                }
            }
            None => Ok(Vec::new()),
        }
    }

    /// Get all active (non-deleted) chunks
    pub fn get_all_chunks(&self) -> Result<Vec<Chunk>> {
        let mut chunks = Vec::new();
        // /157 S2: per-row warns replaced by a classified ≤2-line summary;
        // this full scan also feeds the `undecodable_chunks` status counter.
        let mut scan = scan_log::ScanDecodeLog::new(scan_log::CANONICAL_FULL_SCAN);

        for prefix in &[b"chunk:L0:", b"chunk:L1:"] {
            for (key, value) in self.prefix_scan(*prefix) {
                scan.saw_row();
                match self.decode_chunk(&value) {
                    Ok(chunk) if chunk.deleted_at.is_none() => chunks.push(chunk),
                    Ok(_) => {} // soft-deleted, skip
                    Err(e) => scan.record(&key, &e),
                }
            }
        }

        let summary = scan.finish();
        // /159 S2: never publish a pre-attach scan (provider still the default
        // `NoopProvider` while the process is configured for encryption at
        // rest) — its counts are encrypted rows read without a key, not
        // corruption. Dead path after the /159 boot-order fix in
        // loomem-server `main.rs`; kept as a guard against future init-order
        // regressions.
        if self.at_rest_key_present && !self.encryption.is_enabled() {
            warn!(
                "get_all_chunks: pre-attach scan (ignored for status): {} of {} chunk rows undecodable before provider attach",
                summary.undecodable, summary.scanned
            );
        } else {
            match self.last_scan_decode.lock() {
                Ok(mut slot) => *slot = Some(summary),
                // Poison recovery: guarded data is a plain snapshot, safe to
                // overwrite after a panicked holder.
                Err(poisoned) => *poisoned.into_inner() = Some(summary),
            }
        }

        debug!("Retrieved {} total chunks", chunks.len());
        Ok(chunks)
    }

    /// /157 S3: decode summary of the last full chunk scan, if one ran yet.
    pub fn last_scan_decode_summary(&self) -> Option<scan_log::ScanDecodeSummary> {
        match self.last_scan_decode.lock() {
            Ok(slot) => slot.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    /// Returns a deduplicated, sorted list of `stream` IDs that have at
    /// least one non-tombstoned chunk in `chunk:L0:` or `chunk:L1:` storage.
    ///
    /// Used by the clustering scheduler to discover *all* active streams
    /// (multi-tenant `__user_<uuid>` plus legacy fixed-id namespaces) at
    /// runtime, instead of reading the hardcoded `[namespaces]` config —
    /// which only enumerates pre-multi-tenancy fixed namespaces and never
    /// learns about per-user streams created post-migration.
    pub fn list_active_streams(&self) -> Result<Vec<String>> {
        let mut streams: std::collections::HashSet<String> = std::collections::HashSet::new();

        let mut scan = scan_log::ScanDecodeLog::new("list_active_streams");
        for prefix in &[b"chunk:L0:" as &[u8], b"chunk:L1:"] {
            for (key, value) in self.prefix_scan(prefix) {
                scan.saw_row();
                match self.decode_chunk(&value) {
                    Ok(chunk) if chunk.deleted_at.is_none() => {
                        streams.insert(chunk.stream);
                    }
                    Ok(_) => {}
                    Err(e) => scan.record(&key, &e),
                }
            }
        }
        let _ = scan.finish();

        let mut out: Vec<String> = streams.into_iter().collect();
        out.sort();
        debug!("list_active_streams: discovered {} streams", out.len());
        Ok(out)
    }

    /// Get schema version
    pub fn get_schema_version(&self) -> Result<u32> {
        let key = SCHEMA_VERSION_KEY;
        match self.db.get(key)? {
            Some(bytes) => {
                let version_str = String::from_utf8_lossy(&bytes);
                version_str
                    .parse::<u32>()
                    .with_context(|| format!("Failed to parse schema version: {}", version_str))
            }
            None => Ok(1), // Default to version 1 if not set
        }
    }

    /// Set schema version
    pub fn set_schema_version(&self, version: u32) -> Result<()> {
        let key = SCHEMA_VERSION_KEY;
        let value = version.to_string();
        self.db
            .put(key, value.as_bytes())
            .context("Failed to set schema version")?;
        info!("Set schema version to {}", version);
        Ok(())
    }

    /// Whether a Tantivy full rebuild is pending (e.g., set by loomem-migrate
    /// after restamping chunk.stream fields in RocksDB).
    pub fn get_tantivy_rebuild_needed(&self) -> Result<bool> {
        let key = TANTIVY_REBUILD_FLAG_KEY;
        match self.db.get(key)? {
            Some(bytes) => Ok(bytes.as_slice() == b"1"),
            None => Ok(false),
        }
    }

    /// Set/clear the Tantivy rebuild flag.
    pub fn set_tantivy_rebuild_needed(&self, needed: bool) -> Result<()> {
        let key = TANTIVY_REBUILD_FLAG_KEY;
        let value = if needed { "1" } else { "0" };
        self.db
            .put(key, value.as_bytes())
            .context("Failed to set tantivy_rebuild_needed flag")?;
        Ok(())
    }

    /// Get all event keys for rebuilding index
    pub fn get_all_events(&self) -> Result<Vec<(String, serde_json::Value)>> {
        let prefix = b"event:";
        let mut events = Vec::new();

        for (key, value) in self.prefix_scan(prefix) {
            let key_str = String::from_utf8_lossy(&key);
            match serde_json::from_slice::<serde_json::Value>(&value) {
                Ok(event) => events.push((key_str.to_string(), event)),
                Err(e) => {
                    warn!("Failed to deserialize event {}: {}", key_str, e);
                }
            }
        }

        debug!("Retrieved {} events for rebuild", events.len());
        Ok(events)
    }

    /// Soft-delete a memory — sets deleted_at timestamp, keeps data for recovery window.
    /// Callers should also remove from Tantivy (no soft-delete in FTS) and clean graph refs.
    pub fn delete_by_id(&self, id: &str) -> Result<bool> {
        match self.get_chunk(id)? {
            Some(mut chunk) => {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                chunk.deleted_at = Some(now);
                self.store_chunk(&chunk)?;
                info!("Soft-deleted memory: {}", id);
                Ok(true)
            }
            None => {
                warn!("Memory not found for soft-delete: {}", id);
                Ok(false)
            }
        }
    }

    /// Find all soft-deleted chunks whose recovery window has expired.
    /// Returns chunk IDs where `deleted_at + retention_days < now`.
    pub fn find_expired_soft_deleted(&self, retention_days: u64) -> Result<Vec<String>> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let cutoff = now.saturating_sub(retention_days * 86400);
        let mut expired = Vec::new();

        for level in 0..=2 {
            let prefix = format!("chunk:L{}:", level);
            for item in self.db.prefix_iterator(prefix.as_bytes()) {
                let (_key, value) = match item {
                    Ok(kv) => kv,
                    Err(_) => continue,
                };
                if let Ok(chunk) = self.decode_chunk(&value) {
                    if let Some(deleted_at) = chunk.deleted_at {
                        if deleted_at <= cutoff {
                            expired.push(chunk.id);
                        }
                    }
                }
            }
        }

        Ok(expired)
    }

    /// Hard-delete a memory — physically removes chunk, embedding, entities, and relations.
    /// Used by the retention purge worker after the recovery window expires.
    pub fn hard_delete_by_id(&self, id: &str) -> Result<bool> {
        let mut found = false;

        // Try all levels for chunk keys
        for level in 0..=2 {
            let chunk_key = format!("chunk:L{}:{}", level, id);
            if self.db.get(chunk_key.as_bytes())?.is_some() {
                self.db
                    .delete(chunk_key.as_bytes())
                    .context("Failed to delete chunk")?;
                found = true;
                debug!("Hard-deleted chunk: {}", chunk_key);
                break;
            }
        }

        // Delete from embeddings column family
        let cf = self
            .db
            .cf_handle(CF_EMBEDDINGS)
            .context("Embeddings column family not found")?;
        if self.db.get_cf(&cf, id.as_bytes())?.is_some() {
            self.db
                .delete_cf(&cf, id.as_bytes())
                .context("Failed to delete embedding")?;
        }

        // Delete entities
        let entity_key = format!("entity:{}", id);
        if self.db.get(entity_key.as_bytes())?.is_some() {
            self.db.delete(entity_key.as_bytes())?;
        }

        // Delete relations
        let rel_key = format!("rel:{}", id);
        if self.db.get(rel_key.as_bytes())?.is_some() {
            self.db.delete(rel_key.as_bytes())?;
        }

        if found {
            info!("Hard-deleted memory: {}", id);
        }

        Ok(found)
    }

    /// Purge all memories in a namespace (stream)
    /// Returns list of deleted IDs
    pub fn purge_namespace(&self, stream: &str, dry_run: bool) -> Result<Vec<String>> {
        let mut deleted_ids = Vec::new();
        let mut scan = scan_log::ScanDecodeLog::new("purge_namespace");

        // Scan all chunk levels for matching stream
        for level in 0..=2 {
            let prefix = format!("chunk:L{}:", level);
            for (key, value) in self.prefix_scan(prefix.as_bytes()) {
                scan.saw_row();
                match self.decode_chunk(&value) {
                    Ok(chunk) => {
                        if chunk.stream == stream {
                            deleted_ids.push(chunk.id.clone());

                            if !dry_run {
                                // Delete chunk
                                self.db.delete(&key).context("Failed to delete chunk")?;

                                // Delete embedding
                                let cf = self
                                    .db
                                    .cf_handle(CF_EMBEDDINGS)
                                    .context("Embeddings column family not found")?;
                                let _ = self.db.delete_cf(&cf, chunk.id.as_bytes());

                                // Delete entities
                                let entity_key = format!("entity:{}", chunk.id);
                                let _ = self.db.delete(entity_key.as_bytes());

                                // Delete relations
                                let rel_key = format!("rel:{}", chunk.id);
                                let _ = self.db.delete(rel_key.as_bytes());
                            }
                        }
                    }
                    Err(e) => scan.record(&key, &e),
                }
            }
        }
        let _ = scan.finish();

        if dry_run {
            info!(
                "Dry run: would delete {} memories from stream {}",
                deleted_ids.len(),
                stream
            );
        } else {
            info!(
                "Deleted {} memories from stream {}",
                deleted_ids.len(),
                stream
            );
        }

        Ok(deleted_ids)
    }

    // ── User store (multi-tenant auth) ─────────────────────────

    pub fn store_user(&self, user: &User) -> Result<()> {
        let id_key = format!("user:id:{}", user.id);
        let value = serde_json::to_vec(user).context("Failed to serialize user")?;

        // Clean up stale key indices left behind when a key field changes
        // (rotation, legacy → shared migration, admin disable flipping
        // private_api_key to None). Without this cleanup, stale indices
        // would still resolve to the user and bypass disable/rotate intent.
        if let Ok(Some(prev)) = self.get_user_by_id(&user.id) {
            if let Some(prev_api) = prev.api_key {
                if user.api_key.as_deref() != Some(&prev_api) {
                    let _ = self.db.delete(format!("user:key:{prev_api}").as_bytes());
                }
            }
            if let Some(prev_shared) = prev.shared_api_key {
                if user.shared_api_key.as_deref() != Some(&prev_shared) {
                    let _ = self
                        .db
                        .delete(format!("user:shared_key:{prev_shared}").as_bytes());
                }
            }
            if let Some(prev_private) = prev.private_api_key {
                if user.private_api_key.as_deref() != Some(&prev_private) {
                    let _ = self
                        .db
                        .delete(format!("user:private_key:{prev_private}").as_bytes());
                }
            }
        }

        // Keep all three key→user indices synchronised. Invite-flow users
        // (no keys) get only the id index.
        if let Some(ref api_key) = user.api_key {
            self.db
                .put(format!("user:key:{api_key}").as_bytes(), &value)?;
        }
        if let Some(ref shared) = user.shared_api_key {
            self.db
                .put(format!("user:shared_key:{shared}").as_bytes(), &value)?;
        }
        if let Some(ref private_key) = user.private_api_key {
            self.db
                .put(format!("user:private_key:{private_key}").as_bytes(), &value)?;
        }
        self.db.put(id_key.as_bytes(), &value)?;
        Ok(())
    }

    pub fn get_user_by_key(&self, api_key: &str) -> Result<Option<User>> {
        let key = format!("user:key:{}", api_key);
        match self.db.get(key.as_bytes())? {
            Some(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
            None => Ok(None),
        }
    }

    /// Resolve a shared-scope API key to its owning user.
    /// Middleware uses this as the first lookup after admin-token check (D6 v2).
    pub fn get_user_by_shared_key(&self, api_key: &str) -> Result<Option<User>> {
        let key = format!("user:shared_key:{api_key}");
        match self.db.get(key.as_bytes())? {
            Some(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
            None => Ok(None),
        }
    }

    /// Resolve a private-scope API key to its owning user. The caller is
    /// responsible for verifying `UserFlags.private_stream.active == true`
    /// before trusting the result (§D6 v2 defensive check) — the index
    /// should already be gone when the flag flips off, but middleware
    /// double-checks to defend against a race with admin disable.
    pub fn get_user_by_private_key(&self, api_key: &str) -> Result<Option<User>> {
        let key = format!("user:private_key:{api_key}");
        match self.db.get(key.as_bytes())? {
            Some(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
            None => Ok(None),
        }
    }

    pub fn get_user_by_id(&self, id: &str) -> Result<Option<User>> {
        let key = format!("user:id:{}", id);
        match self.db.get(key.as_bytes())? {
            Some(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
            None => Ok(None),
        }
    }

    pub fn list_users(&self) -> Result<Vec<User>> {
        let mut users = Vec::new();
        for (_, value) in self.prefix_scan(b"user:id:") {
            if let Ok(user) = serde_json::from_slice::<User>(&value) {
                users.push(user);
            }
        }
        Ok(users)
    }

    pub fn delete_user(&self, id: &str) -> Result<bool> {
        if let Some(user) = self.get_user_by_id(id)? {
            if let Some(ref api_key) = user.api_key {
                self.db.delete(format!("user:key:{api_key}").as_bytes())?;
            }
            if let Some(ref shared) = user.shared_api_key {
                self.db
                    .delete(format!("user:shared_key:{shared}").as_bytes())?;
            }
            if let Some(ref private_key) = user.private_api_key {
                self.db
                    .delete(format!("user:private_key:{private_key}").as_bytes())?;
            }
            self.db.delete(format!("user:id:{}", user.id).as_bytes())?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Find a user by email address (linear scan — small user count expected).
    pub fn find_user_by_email(&self, email: &str) -> Result<Option<User>> {
        let email_lower = email.to_lowercase();
        let users = self.list_users()?;
        Ok(users
            .into_iter()
            .find(|u| u.email.as_deref().map(str::to_lowercase).as_deref() == Some(&email_lower)))
    }

    /// Find a user by external identity provider ID.
    pub fn find_user_by_external_id(&self, external_id: &str) -> Result<Option<User>> {
        let users = self.list_users()?;
        Ok(users
            .into_iter()
            .find(|u| u.external_id.as_deref() == Some(external_id)))
    }

    // ── Per-user admin flags (cycle B2) ────────────────────────────
    // Stored under `user_flags:{user_id}` on the default CF. Value is JSON
    // decided by the caller — this layer does no schema enforcement, just
    // raw bytes in/out.

    /// Upsert the flags blob for a user. `value` is the already-serialized
    /// JSON bytes (caller controls the schema — see `audit::UserFlags`).
    pub fn set_user_flags(&self, user_id: &str, value: &[u8]) -> Result<()> {
        let key = format!("user_flags:{user_id}");
        self.put(key.as_bytes(), value)
    }

    /// Return the flags blob for a user, or None if never set.
    pub fn get_user_flags(&self, user_id: &str) -> Result<Option<Vec<u8>>> {
        let key = format!("user_flags:{user_id}");
        self.get(key.as_bytes())
    }

    // ── Audit events (cycle B2) ────────────────────────────────────
    // Append-only log, one row per event. Key: `audit:{user_id}:{ts:020}:{seq:06}`.
    // - `{ts:020}` is unix seconds left-padded to 20 chars so RocksDB's
    //   lexicographic order == chronological order (scan gives ascending time).
    // - `{seq:06}` is a 6-digit counter supplied by the caller (see
    //   `audit::append`) to break ties when two events land in the same second.

    /// Raw append — caller provides the key suffix. Keeps the storage layer
    /// schema-agnostic; shape of `value` is owned by the `audit` module.
    pub fn append_audit(
        &self,
        target_user_id: &str,
        timestamp: u64,
        seq: u32,
        value: &[u8],
    ) -> Result<()> {
        let key = format!("audit:{target_user_id}:{timestamp:020}:{seq:06}");
        // Cycle /134 §C: encrypt the audit event at rest. The per-user audit log
        // is its own DEK scope keyed by `target_user_id` (NoopProvider = no-op).
        let value = self
            .encryption
            .encrypt(target_user_id, value)
            .context("Failed to encrypt audit event")?;
        self.put(key.as_bytes(), &value)
    }

    /// Return the decrypted bytes of the **most recent** audit event for
    /// `target_user_id`, or `None` if the user has no audit history (or the last
    /// row could not be decrypted). /150d: used by `audit::append` to link each
    /// event to its predecessor (tamper-evidence hash-chain). Keys sort
    /// chronologically, so the last item in the prefix scan is the newest.
    pub fn last_audit(&self, target_user_id: &str) -> Option<Vec<u8>> {
        let prefix = format!("audit:{target_user_id}:");
        let v = self.last_value_under_prefix(prefix.as_bytes())?;
        if crate::crypto::at_rest::is_encrypted(&v) {
            self.encryption.decrypt(target_user_id, &v).ok()
        } else {
            Some(v)
        }
    }

    /// Value of the **highest** key sharing `prefix`, via a single reverse seek
    /// — O(1) on the iterator, vs the O(n) `prefix_scan().last()` that walks
    /// every prior key (Greptile #251: this sits on the audit write path).
    ///
    /// Seeks from the smallest key strictly greater than every prefix key (the
    /// prefix with its last byte incremented) and steps backward once. Audit
    /// prefixes end in `:` (0x3a) so the increment never overflows; the generic
    /// 0xFF tail / empty-prefix case falls back to the forward scan for safety.
    fn last_value_under_prefix(&self, prefix: &[u8]) -> Option<Vec<u8>> {
        let mut upper = prefix.to_vec();
        match upper.last_mut() {
            Some(b) if *b != 0xff => *b += 1,
            _ => return self.prefix_scan(prefix).last().map(|(_, v)| v.to_vec()),
        }
        let mut it = self
            .db
            .iterator(IteratorMode::From(&upper, rocksdb::Direction::Reverse));
        match it.next() {
            Some(Ok((k, v))) if k.starts_with(prefix) => Some(v.to_vec()),
            _ => None,
        }
    }

    /// Scan — returns decrypted event bytes for each event in ascending
    /// (timestamp, seq) order, newest LAST, plus a count of rows that were
    /// scanned but could not be decrypted. `limit` caps the number of rows;
    /// pass `usize::MAX` for "all". Caller is responsible for reversing to get
    /// newest-first. /138 §D: encrypted rows (Pattern D, scope =
    /// `target_user_id`) are decrypted here.
    ///
    /// Undecryptable rows (DEK loss, corruption, wrong provider) are dropped
    /// from the returned slice but counted in the returned `usize`, so callers
    /// can tell an incomplete log from an empty one — an audit log that
    /// silently shrinks cannot be trusted.
    pub fn scan_audit(&self, target_user_id: &str, limit: usize) -> (Vec<Vec<u8>>, usize) {
        let prefix = format!("audit:{target_user_id}:");
        let mut events = Vec::new();
        let mut dropped = 0usize;
        for (_, v) in self.prefix_scan(prefix.as_bytes()).take(limit) {
            if crate::crypto::at_rest::is_encrypted(&v) {
                // /138 §D: per-user audit DEK scope = target_user_id (mirrors
                // the §C `append_audit` write scope).
                match self.encryption.decrypt(target_user_id, &v) {
                    Ok(plain) => events.push(plain),
                    Err(e) => {
                        tracing::error!("Failed to decrypt audit event: {e}");
                        dropped += 1;
                    }
                }
            } else {
                // Legacy fall-through: pre-/134 §C plaintext.
                events.push(v.to_vec());
            }
        }
        (events, dropped)
    }

    // ── Access audit (ADR-018, cycle /150e) ───────────────────────────
    // Per-stream data-plane access log. Key: `access:{stream}:{ts:020}:{seq:06}`
    // (same chronological-sort trick as `audit:*`). Encrypted at rest with
    // scope = `stream` (D8) — distinct scope/lifecycle from the per-user
    // `audit:*` admin log. Schema-agnostic: shape of `value` is owned by the
    // `access_audit` module.

    /// Raw append for an access record. Encrypts with scope = `stream`.
    pub fn append_access(
        &self,
        stream: &str,
        timestamp: u64,
        seq: u32,
        value: &[u8],
    ) -> Result<()> {
        let key = format!("access:{stream}:{timestamp:020}:{seq:06}");
        let value = self
            .encryption
            .encrypt(stream, value)
            .context("Failed to encrypt access record")?;
        self.put(key.as_bytes(), &value)
    }

    /// Scan access records for `stream` in ascending (timestamp, seq) order,
    /// newest LAST, plus a count of rows that could not be decrypted. Mirrors
    /// `scan_audit` (scope = `stream`); caller reverses for newest-first.
    pub fn scan_access(&self, stream: &str, limit: usize) -> (Vec<Vec<u8>>, usize) {
        let prefix = format!("access:{stream}:");
        let mut records = Vec::new();
        let mut dropped = 0usize;
        for (_, v) in self.prefix_scan(prefix.as_bytes()).take(limit) {
            if crate::crypto::at_rest::is_encrypted(&v) {
                match self.encryption.decrypt(stream, &v) {
                    Ok(plain) => records.push(plain),
                    Err(e) => {
                        tracing::error!("Failed to decrypt access record: {e}");
                        dropped += 1;
                    }
                }
            } else {
                records.push(v.to_vec());
            }
        }
        (records, dropped)
    }
}

/// Role assigned to a user. Replaces the legacy string-based role field.
/// Backward-compat deserialization via `deserialize_role_legacy` accepts
/// both new format ("admin"/"writer"/"reader") and legacy strings
/// ("global_admin"/"tenant_admin"/"user").
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UserRole {
    #[default]
    Reader,
    Writer,
    Admin,
}

impl UserRole {
    pub fn is_admin(&self) -> bool {
        matches!(self, Self::Admin)
    }

    pub fn can_write(&self) -> bool {
        matches!(self, Self::Writer | Self::Admin)
    }

    /// Shared-scope delete gate — Admin-only per brief §D5.
    pub fn can_delete_shared(&self) -> bool {
        matches!(self, Self::Admin)
    }

    /// Shared-scope dream gate — Admin-only per brief §D5.
    pub fn can_dream_shared(&self) -> bool {
        matches!(self, Self::Admin)
    }

    /// Map legacy role strings to enum. Unknown strings default to Reader
    /// and emit a tracing::error! to surface unexpected values.
    pub fn from_legacy_str(s: &str) -> Self {
        match s {
            "global_admin" | "tenant_admin" | "admin" => Self::Admin,
            "writer" => Self::Writer,
            "reader" | "user" => Self::Reader,
            unknown => {
                tracing::error!(
                    "Unknown role string '{}' during deserialization — defaulting to Reader",
                    unknown
                );
                Self::Reader
            }
        }
    }
}

fn deserialize_role_legacy<'de, D: serde::Deserializer<'de>>(d: D) -> Result<UserRole, D::Error> {
    // Accept both enum format ("admin"/"writer"/"reader") and legacy strings
    // ("global_admin"/"tenant_admin"/"user"). Unknown → Reader (soft fail).
    let v = serde_json::Value::deserialize(d)?;
    if let Some(s) = v.as_str() {
        Ok(UserRole::from_legacy_str(s))
    } else {
        Ok(UserRole::Reader)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    pub id: String,
    /// Legacy API key — pre-C1 single-scope auth. Retained for backward
    /// compatibility (D7): when a pre-existing RocksDB user has this set and
    /// `shared_api_key` empty, middleware treats it as a shared-scope key.
    /// Post-C1 signups leave this `None`; the `shared_api_key` field is the
    /// source of truth. Deprecation happens in follow-up F-C1-2.
    #[serde(default)]
    pub api_key: Option<String>,
    /// Shared workspace scope key — always set post-C1 signup. When the
    /// middleware resolves a request with this key, `stream_id` is derived
    /// as `DEFAULT_STREAM_ID` and role-based gates apply per brief §D5.
    #[serde(default)]
    pub shared_api_key: Option<String>,
    /// Private per-user scope key — set only when `UserFlags.private_stream`
    /// is active (enable flow; admin force-enable). Disable rotates this to
    /// `None` without deleting the underlying stream data (§D9 v2). When the
    /// middleware resolves a request with this key, `stream_id` is derived
    /// as `user.stream_id` and the caller gets owner-level rights.
    #[serde(default)]
    pub private_api_key: Option<String>,
    pub stream_id: String,
    pub created_at: u64,
    pub last_active: Option<u64>,
    pub label: Option<String>,
    pub active: bool,
    /// Tenant isolation boundary. Defaults to stream_id for backward compat.
    #[serde(default)]
    pub workspace_id: Option<String>,
    /// User role — enum replaces legacy string. Backward-compat via
    /// `deserialize_role_legacy` which maps "global_admin"/"tenant_admin"/"user"
    /// to Admin/Admin/Reader respectively.
    #[serde(default, deserialize_with = "deserialize_role_legacy")]
    pub role: UserRole,
    /// Email address (used by SSO providers to link an external identity).
    #[serde(default)]
    pub email: Option<String>,
    /// Display name from identity provider.
    #[serde(default)]
    pub display_name: Option<String>,
    /// External identity provider ID.
    #[serde(default)]
    pub external_id: Option<String>,
    /// True for invite-flow users before their first SSO login.
    #[serde(default)]
    pub pending_first_login: bool,
    /// Unix timestamp of the last successful SSO login.
    /// None for token-only users and legacy users.
    #[serde(default)]
    pub last_login_at: Option<u64>,
}

impl User {
    /// Effective workspace — falls back to stream_id for legacy users.
    pub fn effective_workspace(&self) -> &str {
        self.workspace_id.as_deref().unwrap_or(&self.stream_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RocksDbConfig;
    use tempfile::TempDir;

    fn test_config() -> RocksDbConfig {
        RocksDbConfig {
            max_open_files: 100,
            compression: "lz4".to_string(),
            write_buffer_size: 4 * 1024 * 1024,
            max_write_buffer_number: 2,
        }
    }

    // ── /151 (port of /114b1): ExtractionMeta::event_date_unix ──

    fn extraction_meta_with_date(date: Option<&str>) -> ExtractionMeta {
        ExtractionMeta {
            fact_type: FactType::Fact,
            subject: None,
            event_date: date.map(|s| s.to_string()),
            event_date_context: None,
            supersedes: None,
            superseded_by: None,
            confidence: 0.9,
            extracted_from: None,
            extraction_model: None,
            original_content: None,
            topic: None,
        }
    }

    #[test]
    fn extraction_meta_event_date_unix_iso_date() {
        let m = extraction_meta_with_date(Some("1992-12-01"));
        // 1992-12-01 00:00:00 UTC = 723168000
        assert_eq!(m.event_date_unix(), Some(723_168_000));
    }

    #[test]
    fn extraction_meta_event_date_unix_none_when_missing() {
        let m = extraction_meta_with_date(None);
        assert_eq!(m.event_date_unix(), None);
    }

    #[test]
    fn extraction_meta_event_date_unix_none_when_unparseable() {
        let m = extraction_meta_with_date(Some("not a date"));
        assert_eq!(m.event_date_unix(), None);
    }

    #[test]
    fn extraction_meta_event_date_unix_handles_whitespace() {
        let m = extraction_meta_with_date(Some("  2026-03-15  "));
        // 2026-03-15 00:00:00 UTC
        assert!(m.event_date_unix().is_some());
    }

    #[test]
    fn extraction_meta_event_date_unix_none_when_pre_1970() {
        // Negative unix time cannot land in the u64 valid_from field —
        // u64::try_from path (CLAUDE.md §3, no `as` cast) yields None.
        let m = extraction_meta_with_date(Some("1956-06-15"));
        assert_eq!(m.event_date_unix(), None);
    }

    // /151 AC-5: a chunk persisted without any event_date (extraction_meta =
    // None) round-trips through store_chunk/get_chunk without error and keeps
    // its ingest-timestamp valid_from — backward compat for pre-/151 data.
    #[test]
    fn chunk_without_event_date_reads_back_ok() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let config = test_config();
        let store = RocksDbStore::open(temp_dir.path(), &config)?;

        let chunk = Chunk {
            id: "no-event-date".to_string(),
            content: "legacy chunk".to_string(),
            stream: "s".to_string(),
            level: 0,
            score: 1.0,
            timestamp: 1000,
            consolidated: false,
            dormant: false,
            in_progress: false,
            prompt_version: None,
            source_ids: None,
            last_decay: None,
            metadata: None,
            importance: None,
            persistent: false,
            last_implicit_boost: None,
            access_count: 0,
            source: None,
            created_by: None,
            updated_at: None,
            valid_from: Some(1000),
            valid_until: None,
            is_latest: true,
            superseded_by: None,
            supersedes_id: None,
            root_memory_id: None,
            version: 1,
            memory_type: None,
            extraction_meta: None,
            deleted_at: None,
            trust_level: None,
            ingester_user_id: None,
            alpha: 1.0,
            beta: 1.0,
            harmful_count: 0,
            n_ratings: 0,
            last_rated_at: None,
        };
        store.store_chunk(&chunk)?;

        let read = store
            .get_chunk("no-event-date")?
            .expect("chunk must be readable");
        assert_eq!(read.valid_from, Some(1000));
        assert!(read.extraction_meta.is_none());
        Ok(())
    }

    // Cycle /134 §B AC-B1: a pre-/134 database (no `keys` CF) must open
    // successfully and gain the CF via `create_missing_column_families(true)`.
    #[test]
    fn open_creates_keys_cf_on_legacy_db() {
        use rocksdb::{Options, DB};
        let tmp = TempDir::new().expect("tempdir");
        // Simulate a pre-/134 database: only the legacy CF set, no `keys`.
        {
            let mut opts = Options::default();
            opts.create_if_missing(true);
            opts.create_missing_column_families(true);
            let legacy =
                DB::open_cf(&opts, tmp.path(), [CF_EMBEDDINGS, CF_COSTS]).expect("open legacy db");
            assert!(
                legacy.cf_handle(CF_KEYS).is_none(),
                "legacy db must not have a keys CF"
            );
        }
        // Reopen through RocksDbStore::open, which lists `keys` + create_missing.
        let store = RocksDbStore::open(tmp.path(), &test_config()).expect("reopen pre-/134 db");
        assert!(
            store.db().cf_handle(CF_KEYS).is_some(),
            "keys CF auto-created when opening a pre-/134 database"
        );
    }

    fn make_user(id: &str) -> User {
        User {
            id: id.into(),
            api_key: None,
            shared_api_key: None,
            private_api_key: None,
            stream_id: format!("s_{id}"),
            created_at: 1_000,
            last_active: None,
            label: None,
            active: true,
            workspace_id: None,
            role: UserRole::Reader,
            email: None,
            display_name: None,
            external_id: None,
            pending_first_login: false,
            last_login_at: None,
        }
    }

    #[test]
    fn store_user_keeps_three_indices_in_sync() -> Result<()> {
        let tmp = TempDir::new()?;
        let store = RocksDbStore::open(tmp.path(), &test_config())?;
        let mut user = make_user("u1");
        user.api_key = Some("legacy_aaa".into());
        user.shared_api_key = Some("shared_bbb".into());
        user.private_api_key = Some("private_ccc".into());
        store.store_user(&user)?;

        // All three lookups resolve.
        assert!(store.get_user_by_key("legacy_aaa")?.is_some());
        assert!(store.get_user_by_shared_key("shared_bbb")?.is_some());
        assert!(store.get_user_by_private_key("private_ccc")?.is_some());
        Ok(())
    }

    #[test]
    fn store_user_drops_private_index_when_key_cleared() -> Result<()> {
        let tmp = TempDir::new()?;
        let store = RocksDbStore::open(tmp.path(), &test_config())?;
        let mut user = make_user("u2");
        user.shared_api_key = Some("shared_x".into());
        user.private_api_key = Some("private_y".into());
        store.store_user(&user)?;
        assert!(store.get_user_by_private_key("private_y")?.is_some());

        // Admin disable flow: private_api_key cleared (§D9 v2).
        let mut disabled = user.clone();
        disabled.private_api_key = None;
        store.store_user(&disabled)?;

        assert!(store.get_user_by_private_key("private_y")?.is_none());
        // Shared stays intact.
        assert!(store.get_user_by_shared_key("shared_x")?.is_some());
        Ok(())
    }

    #[test]
    fn store_user_rotates_shared_index() -> Result<()> {
        let tmp = TempDir::new()?;
        let store = RocksDbStore::open(tmp.path(), &test_config())?;
        let mut user = make_user("u3");
        user.shared_api_key = Some("old_key".into());
        store.store_user(&user)?;

        // Rotation → old index removed, new index present.
        let mut rotated = user.clone();
        rotated.shared_api_key = Some("new_key".into());
        store.store_user(&rotated)?;

        assert!(store.get_user_by_shared_key("old_key")?.is_none());
        assert!(store.get_user_by_shared_key("new_key")?.is_some());
        Ok(())
    }

    #[test]
    fn test_basic_operations() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let config = test_config();
        let store = RocksDbStore::open(temp_dir.path(), &config)?;

        // Put
        store.put(b"key1", b"value1")?;
        store.put(b"key2", b"value2")?;

        // Get
        assert_eq!(store.get(b"key1")?, Some(b"value1".to_vec()));
        assert_eq!(store.get(b"key2")?, Some(b"value2".to_vec()));
        assert_eq!(store.get(b"key3")?, None);

        // Delete
        store.delete(b"key1")?;
        assert_eq!(store.get(b"key1")?, None);

        Ok(())
    }

    #[test]
    fn test_scan() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let config = test_config();
        let store = RocksDbStore::open(temp_dir.path(), &config)?;

        store.put(b"a", b"1")?;
        store.put(b"b", b"2")?;
        store.put(b"c", b"3")?;

        let items: Vec<_> = store.scan().collect();
        assert_eq!(items.len(), 3);

        Ok(())
    }

    #[test]
    fn test_prefix_scan() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let config = test_config();
        let store = RocksDbStore::open(temp_dir.path(), &config)?;

        store.put(b"user:1", b"alice")?;
        store.put(b"user:2", b"bob")?;
        store.put(b"post:1", b"hello")?;

        let user_items: Vec<_> = store.prefix_scan(b"user:").collect();
        assert_eq!(user_items.len(), 2);

        let post_items: Vec<_> = store.prefix_scan(b"post:").collect();
        assert_eq!(post_items.len(), 1);

        Ok(())
    }

    // ── Cycle /010 S4: embedding-dimension guard ────────────────────────────

    #[test]
    fn test_embedding_dim_fresh_db_records_dim() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let store = RocksDbStore::open(temp_dir.path(), &test_config())?;

        // Fresh DB: no stored dim, no vectors → any dim is accepted and recorded.
        assert_eq!(store.stored_embedding_dim()?, None);
        store.validate_and_record_embedding_dim(384)?;
        assert_eq!(store.stored_embedding_dim()?, Some(384));

        Ok(())
    }

    #[test]
    fn test_embedding_dim_match_passes_mismatch_fails() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let store = RocksDbStore::open(temp_dir.path(), &test_config())?;

        store.set_embedding_dim(384)?;
        // Same dim → ok.
        assert!(store.validate_and_record_embedding_dim(384).is_ok());
        // Different dim → refuse.
        let err = store
            .validate_and_record_embedding_dim(1536)
            .expect_err("dimension mismatch must error");
        assert!(
            err.to_string().contains("dimension mismatch"),
            "unexpected error: {err}"
        );

        Ok(())
    }

    #[test]
    fn test_embedding_dim_inferred_from_existing_vector() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let store = RocksDbStore::open(temp_dir.path(), &test_config())?;

        // Legacy DB: real 3-dim vector present but no meta key recorded.
        store.store_embedding("legacy-1", vec![0.1f32, 0.2, 0.3])?;
        assert_eq!(store.stored_embedding_dim()?, None);
        assert_eq!(store.sampled_embedding_dim()?, Some(3));

        // Configured dim disagrees with the sampled vector → refuse.
        assert!(store.validate_and_record_embedding_dim(4).is_err());
        // Matching dim → ok and now recorded.
        store.validate_and_record_embedding_dim(3)?;
        assert_eq!(store.stored_embedding_dim()?, Some(3));

        Ok(())
    }

    #[test]
    fn test_recover_orphaned() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let config = test_config();
        let store = RocksDbStore::open(temp_dir.path(), &config)?;

        // Create 3 chunks: 2 orphaned (in_progress=true), 1 normal
        let orphan1 = Chunk {
            id: "orphan-1".to_string(),
            content: "mid-consolidation chunk 1".to_string(),
            stream: "100".to_string(),
            level: 0,
            score: 1.0,
            timestamp: 1000,
            consolidated: false,
            dormant: false,
            in_progress: true, // orphaned
            prompt_version: None,
            source_ids: None,
            last_decay: None,
            metadata: None,
            importance: None,
            persistent: false,
            last_implicit_boost: None,
            access_count: 0,
            source: None,
            created_by: None,
            updated_at: None,
            valid_from: None,
            valid_until: None,
            is_latest: true,
            superseded_by: None,
            supersedes_id: None,
            root_memory_id: None,
            version: 1,
            memory_type: None,
            extraction_meta: None,
            deleted_at: None,
            trust_level: None,
            ingester_user_id: None,
            alpha: 1.0,
            beta: 1.0,
            harmful_count: 0,
            n_ratings: 0,
            last_rated_at: None,
        };
        let orphan2 = Chunk {
            id: "orphan-2".to_string(),
            content: "mid-consolidation chunk 2".to_string(),
            stream: "100".to_string(),
            level: 0,
            score: 1.0,
            timestamp: 1001,
            consolidated: false,
            dormant: false,
            in_progress: true, // orphaned
            prompt_version: None,
            source_ids: None,
            last_decay: None,
            metadata: None,
            importance: None,
            persistent: false,
            last_implicit_boost: None,
            access_count: 0,
            source: None,
            created_by: None,
            updated_at: None,
            valid_from: None,
            valid_until: None,
            is_latest: true,
            superseded_by: None,
            supersedes_id: None,
            root_memory_id: None,
            version: 1,
            memory_type: None,
            extraction_meta: None,
            deleted_at: None,
            trust_level: None,
            ingester_user_id: None,
            alpha: 1.0,
            beta: 1.0,
            harmful_count: 0,
            n_ratings: 0,
            last_rated_at: None,
        };
        let normal = Chunk {
            id: "normal-1".to_string(),
            content: "normal chunk".to_string(),
            stream: "100".to_string(),
            level: 0,
            score: 1.0,
            timestamp: 1002,
            consolidated: false,
            dormant: false,
            in_progress: false, // normal
            prompt_version: None,
            source_ids: None,
            last_decay: None,
            metadata: None,
            importance: None,
            persistent: false,
            last_implicit_boost: None,
            access_count: 0,
            source: None,
            created_by: None,
            updated_at: None,
            valid_from: None,
            valid_until: None,
            is_latest: true,
            superseded_by: None,
            supersedes_id: None,
            root_memory_id: None,
            version: 1,
            memory_type: None,
            extraction_meta: None,
            deleted_at: None,
            trust_level: None,
            ingester_user_id: None,
            alpha: 1.0,
            beta: 1.0,
            harmful_count: 0,
            n_ratings: 0,
            last_rated_at: None,
        };

        store.store_chunk(&orphan1)?;
        store.store_chunk(&orphan2)?;
        store.store_chunk(&normal)?;

        // Simulate restart: recover orphaned
        let recovered = store.recover_orphaned_chunks()?;
        assert_eq!(recovered, 2, "Should recover exactly 2 orphaned chunks");

        // Verify orphans are no longer in_progress
        let key1 = format!("chunk:L0:{}", orphan1.id);
        let data1 = store.get(key1.as_bytes())?.expect("orphan1 should exist");
        let chunk1: Chunk = serde_json::from_slice(&data1)?;
        assert!(
            !chunk1.in_progress,
            "orphan1 should no longer be in_progress"
        );

        // Verify normal chunk unchanged
        let key_n = format!("chunk:L0:{}", normal.id);
        let data_n = store.get(key_n.as_bytes())?.expect("normal should exist");
        let chunk_n: Chunk = serde_json::from_slice(&data_n)?;
        assert!(
            !chunk_n.in_progress,
            "normal chunk should still not be in_progress"
        );

        // Second recovery should find nothing
        let recovered2 = store.recover_orphaned_chunks()?;
        assert_eq!(recovered2, 0, "Second recovery should find 0 orphans");

        Ok(())
    }

    fn make_chunk(id: &str, stream: &str, level: i32) -> Chunk {
        Chunk {
            id: id.to_string(),
            content: format!("content for {id}"),
            stream: stream.to_string(),
            level,
            score: 1.0,
            timestamp: 1000,
            consolidated: false,
            dormant: false,
            in_progress: false,
            prompt_version: None,
            source_ids: None,
            last_decay: None,
            metadata: None,
            importance: None,
            persistent: false,
            last_implicit_boost: None,
            access_count: 0,
            source: None,
            created_by: None,
            updated_at: None,
            valid_from: None,
            valid_until: None,
            is_latest: true,
            superseded_by: None,
            supersedes_id: None,
            root_memory_id: None,
            version: 1,
            memory_type: None,
            extraction_meta: None,
            deleted_at: None,
            trust_level: None,
            ingester_user_id: None,
            alpha: 1.0,
            beta: 1.0,
            harmful_count: 0,
            n_ratings: 0,
            last_rated_at: None,
        }
    }

    // /138 phase 2: field-level encryption write path under MasterKeyEnvProvider.
    // Verifies the envelope is plaintext, the content-bearing fields are cleared,
    // and the encrypted_payload round-trips back to the original tuple.
    #[test]
    fn chunk_field_level_encryption_roundtrip() {
        use crate::crypto::at_rest::MAGIC;
        use crate::crypto::provider::{EncryptionProvider, MasterKeyEnvProvider};
        use std::sync::Arc;

        let tmp = TempDir::new().expect("tempdir");
        let store = RocksDbStore::open(tmp.path(), &test_config()).expect("open");
        let provider = Arc::new(MasterKeyEnvProvider::new([7u8; 32], store.db_arc()));
        let store = store.with_encryption_provider(provider.clone());

        let mut chunk = make_chunk("enc-1", "test-scope-138", 0);
        chunk.content = "hello world".to_string();
        chunk.metadata = Some(serde_json::json!({"k": "v"}));
        chunk.extraction_meta = Some(ExtractionMeta {
            fact_type: FactType::Fact,
            subject: Some("subj".to_string()),
            event_date: None,
            event_date_context: None,
            supersedes: None,
            superseded_by: None,
            confidence: 0.9,
            extracted_from: Some("enc-1".to_string()),
            extraction_model: None,
            original_content: None,
            topic: None,
        });
        store.store_chunk(&chunk).expect("store_chunk");

        // Read the raw RocksDB envelope (plaintext routing fields + payload).
        let key = format!("chunk:L{}:{}", chunk.level, chunk.id);
        let raw = store
            .db()
            .get(key.as_bytes())
            .expect("get")
            .expect("present");

        #[derive(serde::Deserialize)]
        struct Envelope {
            stream: String,
            content: String,
            metadata: Option<serde_json::Value>,
            extraction_meta: Option<serde_json::Value>,
            encrypted_payload: Option<Vec<u8>>,
        }
        let env: Envelope = serde_json::from_slice(&raw).expect("deserialize envelope");

        // Envelope is plaintext; content-bearing fields cleared; payload present.
        assert_eq!(env.stream, "test-scope-138");
        assert!(env.encrypted_payload.is_some());
        assert_eq!(env.content, "");
        assert!(env.metadata.is_none());
        assert!(env.extraction_meta.is_none());

        // Payload is a valid AES-GCM blob (MAGIC prefix).
        let ep = env.encrypted_payload.expect("payload");
        assert_eq!(&ep[..4], &MAGIC[..]);

        // Manual decrypt yields the original (content, metadata, extraction_meta).
        let plaintext = provider.decrypt("test-scope-138", &ep).expect("decrypt");
        let (content, metadata, extraction_meta): (
            String,
            Option<serde_json::Value>,
            Option<ExtractionMeta>,
        ) = serde_json::from_slice(&plaintext).expect("deserialize payload");
        assert_eq!(content, "hello world");
        assert_eq!(metadata, Some(serde_json::json!({"k": "v"})));
        let em = extraction_meta.expect("extraction_meta present");
        assert_eq!(em.subject.as_deref(), Some("subj"));
        assert!((em.confidence - 0.9).abs() < f64::EPSILON);
    }

    // /138 phase 2: with the default NoopProvider the chunk is stored plaintext,
    // with no encrypted_payload — byte-compatible with pre-/138 behavior.
    #[test]
    fn chunk_noop_provider_no_encryption() {
        let tmp = TempDir::new().expect("tempdir");
        let store = RocksDbStore::open(tmp.path(), &test_config()).expect("open");

        let mut chunk = make_chunk("noop-1", "test-scope-138", 0);
        chunk.content = "hello world".to_string();
        store.store_chunk(&chunk).expect("store_chunk");

        let key = format!("chunk:L{}:{}", chunk.level, chunk.id);
        let raw = store
            .db()
            .get(key.as_bytes())
            .expect("get")
            .expect("present");

        #[derive(serde::Deserialize)]
        struct Envelope {
            stream: String,
            content: String,
            encrypted_payload: Option<Vec<u8>>,
        }
        let env: Envelope = serde_json::from_slice(&raw).expect("deserialize envelope");
        assert_eq!(env.content, "hello world");
        assert!(env.encrypted_payload.is_none());
        assert_eq!(env.stream, "test-scope-138");
    }

    // /138 §D AC-D6 #1: end-to-end Pattern A round-trip. store_chunk encrypts
    // under MasterKeyEnvProvider; get_chunk → decode_chunk decrypts and
    // repopulates the cleared content-bearing fields.
    #[test]
    fn chunk_write_then_read_roundtrip() {
        use crate::crypto::provider::MasterKeyEnvProvider;
        use std::sync::Arc;

        let tmp = TempDir::new().expect("tempdir");
        let store = RocksDbStore::open(tmp.path(), &test_config()).expect("open");
        let provider = Arc::new(MasterKeyEnvProvider::new([7u8; 32], store.db_arc()));
        let store = store.with_encryption_provider(provider);

        let mut chunk = make_chunk("rt-1", "scope-rt", 0);
        chunk.content = "round trip content".to_string();
        chunk.metadata = Some(serde_json::json!({"k": "v"}));
        chunk.extraction_meta = Some(ExtractionMeta {
            fact_type: FactType::Fact,
            subject: Some("subj".to_string()),
            event_date: None,
            event_date_context: None,
            supersedes: None,
            superseded_by: None,
            confidence: 0.9,
            extracted_from: Some("rt-1".to_string()),
            extraction_model: None,
            original_content: None,
            topic: None,
        });
        store.store_chunk(&chunk).expect("store_chunk");

        let got = store
            .get_chunk("rt-1")
            .expect("get_chunk")
            .expect("present");
        assert_eq!(got.content, "round trip content");
        assert_eq!(got.metadata, Some(serde_json::json!({"k": "v"})));
        assert_eq!(got.stream, "scope-rt");
        let em = got.extraction_meta.expect("extraction_meta present");
        assert_eq!(em.subject.as_deref(), Some("subj"));
        assert!((em.confidence - 0.9).abs() < f64::EPSILON);
    }

    // /138 §D AC-D6 #2: cross-provider legacy fall-through. A chunk written by
    // the default NoopProvider (plaintext, no encrypted_payload) is read back
    // correctly by an encryption-enabled store via decode_chunk's empty-payload
    // branch — no decrypt attempted.
    #[test]
    fn chunk_legacy_plaintext_fallthrough() {
        use crate::crypto::provider::MasterKeyEnvProvider;
        use std::sync::Arc;

        let tmp = TempDir::new().expect("tempdir");
        let store = RocksDbStore::open(tmp.path(), &test_config()).expect("open");

        let mut chunk = make_chunk("legacy-1", "scope-leg", 0);
        chunk.content = "legacy plaintext".to_string();
        store.store_chunk(&chunk).expect("store_chunk"); // NoopProvider → plaintext

        // Enable encryption over the SAME db, then read the pre-existing row.
        let provider = Arc::new(MasterKeyEnvProvider::new([7u8; 32], store.db_arc()));
        let enc_store = store.with_encryption_provider(provider);
        let got = enc_store
            .get_chunk("legacy-1")
            .expect("get_chunk")
            .expect("present");
        assert_eq!(got.content, "legacy plaintext");
        assert_eq!(got.stream, "scope-leg");
    }

    // /138 §D AC-D6 #5: Pattern D audit whole-blob round-trip. append_audit
    // encrypts under target_user_id; scan_audit decrypts under the same scope.
    #[test]
    fn audit_write_then_read_roundtrip() {
        use crate::crypto::provider::MasterKeyEnvProvider;
        use std::sync::Arc;

        let tmp = TempDir::new().expect("tempdir");
        let store = RocksDbStore::open(tmp.path(), &test_config()).expect("open");
        let provider = Arc::new(MasterKeyEnvProvider::new([7u8; 32], store.db_arc()));
        let store = store.with_encryption_provider(provider);

        let event = br#"{"action":"disable","by":"admin"}"#;
        store
            .append_audit("user-audit-1", 1_000, 1, event)
            .expect("append_audit");

        let (scanned, dropped) = store.scan_audit("user-audit-1", usize::MAX);
        assert_eq!(scanned.len(), 1);
        assert_eq!(scanned[0], event);
        assert_eq!(dropped, 0);
        // Different user's scope sees nothing.
        assert!(store.scan_audit("user-audit-2", usize::MAX).0.is_empty());
    }

    // Review fix (storage.rs:1592): an undecryptable audit row must be counted,
    // not silently dropped — callers need to tell an incomplete log from an
    // empty one. Encrypt under master key A, then read with key B (same DB):
    // the wrapped DEK can no longer be unwrapped, so the row is undecryptable.
    #[test]
    fn audit_scan_counts_undecryptable_rows() {
        use crate::crypto::provider::MasterKeyEnvProvider;
        use std::sync::Arc;

        let tmp = TempDir::new().expect("tempdir");
        let store = RocksDbStore::open(tmp.path(), &test_config()).expect("open");
        let provider_a = Arc::new(MasterKeyEnvProvider::new([1u8; 32], store.db_arc()));
        let store = store.with_encryption_provider(provider_a);

        store
            .append_audit("u_drop", 1_000, 1, br#"{"action":"x"}"#)
            .expect("append_audit");

        // Rewrap the SAME db with a different master key — DEK now unwrappable.
        let provider_b = Arc::new(MasterKeyEnvProvider::new([2u8; 32], store.db_arc()));
        let store = store.with_encryption_provider(provider_b);

        let (events, dropped) = store.scan_audit("u_drop", usize::MAX);
        assert!(events.is_empty());
        assert_eq!(dropped, 1);
    }

    // /138 §D AC-D6 #6: Pattern C entity/rel whole-blob round-trip under
    // MasterKeyEnvProvider; get_entities/get_relations decrypt with the chunk's
    // stream scope.
    #[test]
    fn entities_rel_write_then_read_roundtrip() {
        use crate::crypto::provider::MasterKeyEnvProvider;
        use std::sync::Arc;

        let tmp = TempDir::new().expect("tempdir");
        let store = RocksDbStore::open(tmp.path(), &test_config()).expect("open");
        let provider = Arc::new(MasterKeyEnvProvider::new([7u8; 32], store.db_arc()));
        let store = store.with_encryption_provider(provider);

        store
            .store_entities(
                "chunk-c",
                "scope-c",
                &[("Alice".to_string(), "Person".to_string())],
            )
            .expect("store_entities");
        let ents = store
            .get_entities("chunk-c", "scope-c")
            .expect("get_entities");
        assert_eq!(ents, vec!["Alice".to_string()]);

        store
            .store_relations(
                "chunk-c",
                "scope-c",
                &[("Alice".to_string(), "knows".to_string(), "Bob".to_string())],
            )
            .expect("store_relations");
        let rels = store
            .get_relations("chunk-c", "scope-c")
            .expect("get_relations");
        assert_eq!(
            rels,
            vec![("Alice".to_string(), "knows".to_string(), "Bob".to_string())]
        );
    }

    // Downgrade hazard: an encrypted entity:/rel: row read back under the
    // default NoopProvider (master key removed) must propagate an error rather
    // than emit garbage names via from_utf8_lossy. Mirrors decode_chunk's
    // propagate-don't-corrupt behavior.
    #[test]
    fn encrypted_entities_rel_under_disabled_provider_error() {
        use crate::crypto::provider::{MasterKeyEnvProvider, NoopProvider};
        use std::sync::Arc;

        let tmp = TempDir::new().expect("tempdir");
        let store = RocksDbStore::open(tmp.path(), &test_config()).expect("open");

        // Write encrypted entity:/rel: rows under an enabled provider.
        let provider = Arc::new(MasterKeyEnvProvider::new([7u8; 32], store.db_arc()));
        let enc_store = store.with_encryption_provider(provider);
        enc_store
            .store_entities(
                "chunk-dg",
                "scope-dg",
                &[("Alice".to_string(), "Person".to_string())],
            )
            .expect("store_entities");
        enc_store
            .store_relations(
                "chunk-dg",
                "scope-dg",
                &[("Alice".to_string(), "knows".to_string(), "Bob".to_string())],
            )
            .expect("store_relations");

        // Downgrade: same db, master key gone (default NoopProvider). The
        // encrypted rows are still magic-prefixed, so reads must error rather
        // than pass ciphertext through to from_utf8_lossy.
        let store = enc_store.with_encryption_provider(Arc::new(NoopProvider));
        assert!(
            store.get_entities("chunk-dg", "scope-dg").is_err(),
            "encrypted entity row must error under disabled provider, not emit garbage"
        );
        assert!(
            store.get_relations("chunk-dg", "scope-dg").is_err(),
            "encrypted rel row must error under disabled provider, not emit garbage"
        );
    }

    #[test]
    fn list_active_streams_empty_storage_returns_empty() -> Result<()> {
        let tmp = TempDir::new()?;
        let store = RocksDbStore::open(tmp.path(), &test_config())?;
        assert_eq!(store.list_active_streams()?, Vec::<String>::new());
        Ok(())
    }

    #[test]
    fn list_active_streams_dedups_and_sorts() -> Result<()> {
        let tmp = TempDir::new()?;
        let store = RocksDbStore::open(tmp.path(), &test_config())?;

        // Three streams: legacy fixed-id "100", two multi-tenant user UUIDs.
        // Both L0 and L1 chunks across streams; "__user_aaa" appears in both
        // levels to verify dedup.
        store.store_chunk(&make_chunk("c1", "100", 0))?;
        store.store_chunk(&make_chunk("c2", "__user_aaa", 0))?;
        store.store_chunk(&make_chunk("c3", "__user_aaa", 1))?;
        store.store_chunk(&make_chunk("c4", "__user_bbb", 1))?;

        let streams = store.list_active_streams()?;
        assert_eq!(
            streams,
            vec![
                "100".to_string(),
                "__user_aaa".to_string(),
                "__user_bbb".to_string(),
            ]
        );
        Ok(())
    }

    #[test]
    fn list_active_streams_l0_only_visible_when_l1_empty() -> Result<()> {
        let tmp = TempDir::new()?;
        let store = RocksDbStore::open(tmp.path(), &test_config())?;

        // Only L0 chunks (no consolidation has run yet).
        store.store_chunk(&make_chunk("c1", "__user_xxx", 0))?;
        store.store_chunk(&make_chunk("c2", "__user_yyy", 0))?;

        assert_eq!(
            store.list_active_streams()?,
            vec!["__user_xxx".to_string(), "__user_yyy".to_string()]
        );
        Ok(())
    }

    #[test]
    fn list_active_streams_skips_tombstoned_chunks() -> Result<()> {
        let tmp = TempDir::new()?;
        let store = RocksDbStore::open(tmp.path(), &test_config())?;

        // Stream "ghost" only has a soft-deleted chunk; "live" has a healthy one.
        let mut tombstoned = make_chunk("g1", "ghost", 0);
        tombstoned.deleted_at = Some(2000);
        store.store_chunk(&tombstoned)?;
        store.store_chunk(&make_chunk("l1", "live", 0))?;

        assert_eq!(store.list_active_streams()?, vec!["live".to_string()]);
        Ok(())
    }

    #[test]
    fn test_delete_by_id() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let config = test_config();
        let store = RocksDbStore::open(temp_dir.path(), &config)?;

        // Create a test chunk with embedding and entities
        let chunk = Chunk {
            id: "test-delete-123".to_string(),
            content: "test content".to_string(),
            stream: "100".to_string(),
            level: 0,
            score: 1.0,
            timestamp: 1000,
            consolidated: false,
            dormant: false,
            in_progress: false,
            prompt_version: None,
            source_ids: None,
            last_decay: None,
            metadata: None,
            importance: None,
            persistent: false,
            last_implicit_boost: None,
            access_count: 0,
            source: None,
            created_by: None,
            updated_at: None,
            valid_from: None,
            valid_until: None,
            is_latest: true,
            superseded_by: None,
            supersedes_id: None,
            root_memory_id: None,
            version: 1,
            memory_type: None,
            extraction_meta: None,
            deleted_at: None,
            trust_level: None,
            ingester_user_id: None,
            alpha: 1.0,
            beta: 1.0,
            harmful_count: 0,
            n_ratings: 0,
            last_rated_at: None,
        };

        store.store_chunk(&chunk)?;
        store.store_embedding(&chunk.id, vec![0.1, 0.2, 0.3])?;
        store.store_entities(
            &chunk.id,
            &chunk.stream,
            &[("Entity1".to_string(), "Person".to_string())],
        )?;

        // Verify chunk exists
        assert!(store.get_chunk(&chunk.id)?.is_some());

        // Delete the chunk
        let deleted = store.delete_by_id(&chunk.id)?;
        assert!(deleted, "Should return true for successful deletion");

        // Verify soft-delete contract: chunk still readable, deleted_at set;
        // embedding untouched (hard-delete is a separate code path).
        let soft_deleted = store
            .get_chunk(&chunk.id)?
            .expect("soft-deleted chunk should still be readable via get_chunk");
        assert!(
            soft_deleted.deleted_at.is_some(),
            "chunk.deleted_at should be Some(_) after delete_by_id (soft-delete contract)"
        );
        assert!(store.get_embedding(&chunk.id)?.is_some());

        // Try deleting non-existent chunk
        let deleted2 = store.delete_by_id("non-existent")?;
        assert!(!deleted2, "Should return false for non-existent chunk");

        Ok(())
    }

    #[test]
    fn test_purge_namespace() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let config = test_config();
        let store = RocksDbStore::open(temp_dir.path(), &config)?;

        // Create chunks in different streams
        let chunk1 = Chunk {
            id: "chunk-stream-100-1".to_string(),
            content: "content 1".to_string(),
            stream: "100".to_string(),
            level: 0,
            score: 1.0,
            timestamp: 1000,
            consolidated: false,
            dormant: false,
            in_progress: false,
            prompt_version: None,
            source_ids: None,
            last_decay: None,
            metadata: None,
            importance: None,
            persistent: false,
            last_implicit_boost: None,
            access_count: 0,
            source: None,
            created_by: None,
            updated_at: None,
            valid_from: None,
            valid_until: None,
            is_latest: true,
            superseded_by: None,
            supersedes_id: None,
            root_memory_id: None,
            version: 1,
            memory_type: None,
            extraction_meta: None,
            deleted_at: None,
            trust_level: None,
            ingester_user_id: None,
            alpha: 1.0,
            beta: 1.0,
            harmful_count: 0,
            n_ratings: 0,
            last_rated_at: None,
        };

        let chunk2 = Chunk {
            id: "chunk-stream-100-2".to_string(),
            content: "content 2".to_string(),
            stream: "100".to_string(),
            level: 1,
            score: 1.0,
            timestamp: 1001,
            consolidated: false,
            dormant: false,
            in_progress: false,
            prompt_version: None,
            source_ids: None,
            last_decay: None,
            metadata: None,
            importance: None,
            persistent: false,
            last_implicit_boost: None,
            access_count: 0,
            source: None,
            created_by: None,
            updated_at: None,
            valid_from: None,
            valid_until: None,
            is_latest: true,
            superseded_by: None,
            supersedes_id: None,
            root_memory_id: None,
            version: 1,
            memory_type: None,
            extraction_meta: None,
            deleted_at: None,
            trust_level: None,
            ingester_user_id: None,
            alpha: 1.0,
            beta: 1.0,
            harmful_count: 0,
            n_ratings: 0,
            last_rated_at: None,
        };

        let chunk3 = Chunk {
            id: "chunk-stream-200-1".to_string(),
            content: "content 3".to_string(),
            stream: "200".to_string(),
            level: 0,
            score: 1.0,
            timestamp: 1002,
            consolidated: false,
            dormant: false,
            in_progress: false,
            prompt_version: None,
            source_ids: None,
            last_decay: None,
            metadata: None,
            importance: None,
            persistent: false,
            last_implicit_boost: None,
            access_count: 0,
            source: None,
            created_by: None,
            updated_at: None,
            valid_from: None,
            valid_until: None,
            is_latest: true,
            superseded_by: None,
            supersedes_id: None,
            root_memory_id: None,
            version: 1,
            memory_type: None,
            extraction_meta: None,
            deleted_at: None,
            trust_level: None,
            ingester_user_id: None,
            alpha: 1.0,
            beta: 1.0,
            harmful_count: 0,
            n_ratings: 0,
            last_rated_at: None,
        };

        store.store_chunk(&chunk1)?;
        store.store_chunk(&chunk2)?;
        store.store_chunk(&chunk3)?;

        // Dry run purge stream 100
        let dry_run_ids = store.purge_namespace("100", true)?;
        assert_eq!(dry_run_ids.len(), 2, "Should find 2 chunks in stream 100");

        // Verify chunks still exist after dry run
        assert!(store.get_chunk(&chunk1.id)?.is_some());
        assert!(store.get_chunk(&chunk2.id)?.is_some());
        assert!(store.get_chunk(&chunk3.id)?.is_some());

        // Actually purge stream 100
        let deleted_ids = store.purge_namespace("100", false)?;
        assert_eq!(
            deleted_ids.len(),
            2,
            "Should delete 2 chunks from stream 100"
        );

        // Verify stream 100 chunks are gone, stream 200 remains
        assert!(store.get_chunk(&chunk1.id)?.is_none());
        assert!(store.get_chunk(&chunk2.id)?.is_none());
        assert!(store.get_chunk(&chunk3.id)?.is_some());

        Ok(())
    }

    // ── UserRole unit tests ───────────────────────────────────────────────────

    #[test]
    fn test_user_role_from_legacy_str_admin_variants() {
        assert_eq!(UserRole::from_legacy_str("global_admin"), UserRole::Admin);
        assert_eq!(UserRole::from_legacy_str("tenant_admin"), UserRole::Admin);
        assert_eq!(UserRole::from_legacy_str("admin"), UserRole::Admin);
    }

    #[test]
    fn test_user_role_from_legacy_str_writer() {
        assert_eq!(UserRole::from_legacy_str("writer"), UserRole::Writer);
    }

    #[test]
    fn test_user_role_from_legacy_str_reader_variants() {
        assert_eq!(UserRole::from_legacy_str("reader"), UserRole::Reader);
        assert_eq!(UserRole::from_legacy_str("user"), UserRole::Reader);
    }

    #[test]
    fn test_user_role_from_legacy_str_unknown_defaults_to_reader() {
        // Unknown strings default to Reader (soft fail per approval addendum)
        assert_eq!(UserRole::from_legacy_str("superuser"), UserRole::Reader);
        assert_eq!(UserRole::from_legacy_str(""), UserRole::Reader);
    }

    #[test]
    fn test_user_role_serde_roundtrip_canonical() {
        // Canonical serialization: Reader → "reader", Writer → "writer", Admin → "admin"
        assert_eq!(
            serde_json::to_string(&UserRole::Admin).unwrap(),
            r#""admin""#
        );
        assert_eq!(
            serde_json::to_string(&UserRole::Writer).unwrap(),
            r#""writer""#
        );
        assert_eq!(
            serde_json::to_string(&UserRole::Reader).unwrap(),
            r#""reader""#
        );
    }

    #[test]
    fn test_user_deserialize_legacy_role_string() {
        // User record stored on-disk with legacy role string "global_admin"
        // should deserialize to UserRole::Admin via deserialize_role_legacy.
        let json = r#"{
            "id": "u1",
            "api_key": "loom_abc",
            "stream_id": "s1",
            "created_at": 0,
            "last_active": null,
            "label": null,
            "active": true,
            "role": "global_admin"
        }"#;
        let user: User = serde_json::from_str(json).unwrap();
        assert_eq!(user.role, UserRole::Admin);
    }

    #[test]
    fn test_user_deserialize_legacy_role_user() {
        // Legacy "user" string → Reader
        let json = r#"{
            "id": "u2",
            "api_key": "loom_xyz",
            "stream_id": "s2",
            "created_at": 0,
            "last_active": null,
            "label": null,
            "active": true,
            "role": "user"
        }"#;
        let user: User = serde_json::from_str(json).unwrap();
        assert_eq!(user.role, UserRole::Reader);
        // Verify serialization back to canonical form
        let re_serialized: serde_json::Value = serde_json::to_value(user.role).unwrap();
        assert_eq!(re_serialized, serde_json::json!("reader"));
    }

    #[test]
    fn test_user_deserialize_canonical_role() {
        // New-format "admin" string → Admin
        let json = r#"{
            "id": "u3",
            "api_key": "loom_def",
            "stream_id": "s3",
            "created_at": 0,
            "last_active": null,
            "label": null,
            "active": true,
            "role": "admin"
        }"#;
        let user: User = serde_json::from_str(json).unwrap();
        assert_eq!(user.role, UserRole::Admin);
        let re_serialized: serde_json::Value = serde_json::to_value(user.role).unwrap();
        assert_eq!(re_serialized, serde_json::json!("admin"));
    }

    #[test]
    fn test_user_default_role_is_reader() {
        // When role field absent, default to Reader
        let json = r#"{
            "id": "u4",
            "api_key": "loom_nnn",
            "stream_id": "s4",
            "created_at": 0,
            "last_active": null,
            "label": null,
            "active": true
        }"#;
        let user: User = serde_json::from_str(json).unwrap();
        assert_eq!(user.role, UserRole::Reader);
    }

    // ── Cycle B2 storage methods ──────────────────────────────────

    #[test]
    fn test_user_flags_roundtrip() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let store = RocksDbStore::open(temp_dir.path(), &test_config())?;

        assert!(store.get_user_flags("u1")?.is_none());

        store.set_user_flags("u1", br#"{"private_stream":{"active":true}}"#)?;
        let got = store.get_user_flags("u1")?.expect("flags present");
        assert_eq!(
            std::str::from_utf8(&got).unwrap(),
            r#"{"private_stream":{"active":true}}"#
        );

        // Overwrite
        store.set_user_flags("u1", br#"{"private_stream":{"active":false}}"#)?;
        let got2 = store.get_user_flags("u1")?.expect("flags present");
        assert!(std::str::from_utf8(&got2).unwrap().contains("false"));

        // Isolated per user
        assert!(store.get_user_flags("u2")?.is_none());
        Ok(())
    }

    #[test]
    fn test_audit_append_scan_order() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let store = RocksDbStore::open(temp_dir.path(), &test_config())?;

        // Three events for u1 in ascending ts.
        store.append_audit("u1", 100, 0, b"ev_a")?;
        store.append_audit("u1", 200, 0, b"ev_b")?;
        store.append_audit("u1", 300, 0, b"ev_c")?;
        // One for u2 to confirm isolation.
        store.append_audit("u2", 150, 0, b"ev_other")?;

        let (all, dropped) = store.scan_audit("u1", usize::MAX);
        assert_eq!(all.len(), 3);
        assert_eq!(dropped, 0);
        assert_eq!(all[0], b"ev_a");
        assert_eq!(all[1], b"ev_b");
        assert_eq!(all[2], b"ev_c");

        let (limited, _) = store.scan_audit("u1", 2);
        assert_eq!(limited.len(), 2);
        assert_eq!(limited[0], b"ev_a");

        // u2 isolated.
        let (other, _) = store.scan_audit("u2", usize::MAX);
        assert_eq!(other.len(), 1);
        assert_eq!(other[0], b"ev_other");

        // Unknown user = empty.
        let (none, _) = store.scan_audit("u_missing", usize::MAX);
        assert!(none.is_empty());
        Ok(())
    }

    #[test]
    fn test_audit_seq_breaks_same_second_tie() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let store = RocksDbStore::open(temp_dir.path(), &test_config())?;

        // Two events at the same timestamp, distinct seq.
        store.append_audit("u1", 100, 1, b"first")?;
        store.append_audit("u1", 100, 2, b"second")?;

        let (all, _) = store.scan_audit("u1", usize::MAX);
        assert_eq!(all.len(), 2);
        assert_eq!(all[0], b"first");
        assert_eq!(all[1], b"second");
        Ok(())
    }

    // --- AC-14 zombie guard tests ---

    #[test]
    #[should_panic(expected = "L2 tier removed")]
    fn store_chunk_level_2_panics_in_debug() {
        // debug_assert! fires in debug builds (default for `cargo test`).
        // In release builds this test is a no-op compile-check only.
        let tmp = TempDir::new().unwrap();
        let store = RocksDbStore::open(tmp.path(), &test_config()).unwrap();
        let chunk = make_chunk("zombie-1", "__user_test", 2);
        // Panics with "L2 tier removed" via debug_assert! in encode_chunk (called
        // by store_chunk).
        let _ = store.store_chunk(&chunk);
    }

    #[test]
    fn store_chunk_level_2_emits_warn() {
        // Tests the warn_if_zombie_level helper directly, bypassing store_chunk's
        // debug_assert! so this test works in both debug and release builds.
        // We verify the function completes without panic for level=2 (production
        // warn path) and is a no-op for level=1 (valid path).
        // Log capture requires tracing-subscriber — not a dev-dep; observability
        // of the actual warn line is validated via production logs + the helper
        // existing as a callable unit-tested here.
        let zombie = make_chunk("zombie-2", "__user_test", 2);
        // Must not panic — the warn path is fire-and-continue.
        warn_if_zombie_level(&zombie);

        let valid = make_chunk("valid-1", "__user_test", 1);
        // Must not panic — level=1 is the no-op path.
        warn_if_zombie_level(&valid);
    }

    /// /157 S2 (AC-4): a full scan over undecodable rows classifies them per
    /// failure stage into `last_scan_decode_summary` (warn output is the ≤2
    /// summary lines in `ScanDecodeLog::finish`, never per-row warns), and
    /// still returns every decodable chunk.
    #[test]
    fn get_all_chunks_classifies_undecodable_rows() {
        use crate::crypto::provider::{EncryptionProvider, MasterKeyEnvProvider};
        use std::sync::Arc;

        let tmp = TempDir::new().expect("tempdir");
        let store = RocksDbStore::open(tmp.path(), &test_config()).expect("open");
        let provider = Arc::new(MasterKeyEnvProvider::new([7u8; 32], store.db_arc()));
        let store = store.with_encryption_provider(provider.clone());

        // No scan ran yet → no summary.
        assert!(store.last_scan_decode_summary().is_none());

        // One healthy encrypted chunk.
        let mut good = make_chunk("scan-good", "scan-scope-157", 0);
        good.content = "healthy row".to_string();
        store.store_chunk(&good).expect("store good chunk");

        // Row 2: envelope is not JSON at all → Envelope stage.
        store
            .put(b"chunk:L0:scan-bad-envelope", b"\x00not-json")
            .expect("put bad envelope row");

        // Row 3: /134 §C whole-blob era shape — payload decrypts under the
        // stream DEK but is the whole chunk JSON, not the §D 3-tuple →
        // Payload stage (the incident-B fingerprint).
        let whole = serde_json::to_vec(&good).expect("serialize whole chunk");
        let encrypted = provider
            .encrypt("scan-scope-157", &whole)
            .expect("encrypt whole-blob payload");
        let mut envelope = serde_json::to_value(&good).expect("chunk to value");
        envelope["content"] = serde_json::json!("");
        envelope["metadata"] = serde_json::Value::Null;
        envelope["extraction_meta"] = serde_json::Value::Null;
        envelope["encrypted_payload"] = serde_json::to_value(&encrypted).expect("blob to value");
        store
            .put(
                b"chunk:L0:scan-whole-blob",
                &serde_json::to_vec(&envelope).expect("envelope bytes"),
            )
            .expect("put whole-blob row");

        let chunks = store.get_all_chunks().expect("scan succeeds");
        assert_eq!(chunks.len(), 1, "only the healthy chunk decodes");
        assert_eq!(chunks[0].id, "scan-good");

        let summary = store
            .last_scan_decode_summary()
            .expect("full scan stored a summary");
        assert_eq!(summary.scanned, 3);
        assert_eq!(summary.undecodable, 2);
        assert_eq!(summary.envelope, 1);
        assert_eq!(summary.payload, 1);
        assert_eq!(summary.decrypt, 0);
        let first = summary.first_error.expect("first error chain kept");
        assert!(
            first.contains("Failed to deserialize chunk"),
            "context chain kept: {first}"
        );
    }

    /// /159 AC-2: restart simulation in the fixed boot order — reopen the
    /// store and attach the provider BEFORE the first scan; every encrypted
    /// row decodes and the published summary reports zero undecodable.
    #[test]
    fn boot_order_attach_before_scan_decodes_all_rows() {
        use crate::crypto::provider::MasterKeyEnvProvider;
        use std::sync::Arc;

        let tmp = TempDir::new().expect("tempdir");
        let key = [7u8; 32];
        {
            let store = RocksDbStore::open(tmp.path(), &test_config()).expect("open");
            let provider = Arc::new(MasterKeyEnvProvider::new(key, store.db_arc()));
            let store = store.with_encryption_provider(provider);
            for i in 0..3 {
                let mut chunk = make_chunk(&format!("boot-{i}"), "boot-scope-159", 0);
                chunk.content = format!("row {i}");
                store.store_chunk(&chunk).expect("store chunk");
            }
        }
        // Restart: attach immediately after open (the /159 S1 order), scan after.
        let store = RocksDbStore::open(tmp.path(), &test_config()).expect("reopen");
        let provider = Arc::new(MasterKeyEnvProvider::new(key, store.db_arc()));
        let store = store.with_encryption_provider(provider);
        let chunks = store.get_all_chunks().expect("scan succeeds");
        assert_eq!(chunks.len(), 3, "all encrypted rows decode post-attach");
        let summary = store
            .last_scan_decode_summary()
            .expect("post-attach scan publishes a summary");
        assert_eq!(summary.scanned, 3);
        assert_eq!(summary.undecodable, 0);
    }

    /// /159 AC-3 (S2): a scan that runs before the provider attach while the
    /// process is configured for encryption at rest (master key present) must
    /// NOT publish `last_scan_decode_summary`; the next post-attach scan does.
    #[test]
    fn pre_attach_scan_is_ignored_for_status() {
        use crate::crypto::provider::MasterKeyEnvProvider;
        use std::sync::Arc;

        let tmp = TempDir::new().expect("tempdir");
        let key = [7u8; 32];
        {
            let store = RocksDbStore::open(tmp.path(), &test_config()).expect("open");
            let provider = Arc::new(MasterKeyEnvProvider::new(key, store.db_arc()));
            let store = store.with_encryption_provider(provider);
            let mut chunk = make_chunk("pre-attach-1", "boot-scope-159b", 0);
            chunk.content = "encrypted row".to_string();
            store.store_chunk(&chunk).expect("store chunk");
        }
        // Boot-order regression simulation: reopen WITHOUT attaching the
        // provider; force the key-present snapshot (the field reads the env
        // at open(), and the env is absent under `cargo test`).
        let mut store = RocksDbStore::open(tmp.path(), &test_config()).expect("reopen");
        store.at_rest_key_present = true;
        let chunks = store.get_all_chunks().expect("pre-attach scan succeeds");
        assert!(
            chunks.is_empty(),
            "encrypted rows are unreadable through NoopProvider"
        );
        assert!(
            store.last_scan_decode_summary().is_none(),
            "pre-attach scan must not feed the status counter"
        );
        // Provider attach (the /159 S1 order) → the next scan publishes.
        let provider = Arc::new(MasterKeyEnvProvider::new(key, store.db_arc()));
        let store = store.with_encryption_provider(provider);
        let chunks = store.get_all_chunks().expect("post-attach scan succeeds");
        assert_eq!(chunks.len(), 1);
        let summary = store
            .last_scan_decode_summary()
            .expect("post-attach scan publishes a summary");
        assert_eq!(summary.undecodable, 0);
    }
}
