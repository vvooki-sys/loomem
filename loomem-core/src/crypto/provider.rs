// Cycle /134 §B: pluggable encryption-at-rest provider.
//
// `EncryptionProvider` is the seam the storage layer holds; §C/§D wire the
// actual `encrypt`/`decrypt` calls into read/write paths. In §B the provider
// is constructed and held only.
//
// Two implementations:
//   * `MasterKeyEnvProvider` — real envelope encryption. A master key (from
//     `LOOMEM_AT_REST_MASTER_KEY`, base64 → 32 bytes) wraps a per-scope DEK.
//     DEKs live in the `keys` column family (`scope:{scope}` → wrapped DEK)
//     and are cached in-process (FIFO, 100 entries).
//   * `NoopProvider` — pass-through used when encryption is disabled.
//
// Authoritative blob/wrap format: ADR-013. Primitives: `crypto::at_rest`.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, PoisonError};

use rocksdb::{IteratorMode, DB};

use hmac::{Hmac, Mac};
use serde::Serialize;
use sha2::Sha256;

use crate::crypto::at_rest::{self, CryptoError, WrappedStreamDek};
use crate::storage::CF_KEYS;

/// Env var carrying the base64-encoded 32-byte master key.
pub const MASTER_KEY_ENV: &str = "LOOMEM_AT_REST_MASTER_KEY";

/// In-process DEK cache capacity (FIFO eviction).
const CACHE_CAP: usize = 100;

/// DEK id assigned to the first (only, in §B) DEK generation per scope.
const INITIAL_DEK_ID: u32 = 1;

/// Master key generation. Rotation tooling (§G) bumps this; §B is always v1.
const MASTER_KEY_VERSION: u8 = 1;

/// Domain-separation tag for the non-secret key fingerprint (cycle /144).
/// Distinct from the `"loomem-index-v1"` index MAC-key tag (§4 Decision 4), so
/// the fingerprint is unlinkable to any DEK or index token.
const FINGERPRINT_TAG: &[u8] = b"loomem-key-fingerprint-v1";

/// Length of the hex fingerprint exposed in health/logs (8 hex chars = 4 bytes).
const FINGERPRINT_HEX_LEN: usize = 8;

/// Byte count hashed into the fingerprint (`FINGERPRINT_HEX_LEN` hex chars =
/// this many bytes). Named separately so the hex/byte relationship is explicit
/// and a future edit to one can't silently halve/double the other.
const FINGERPRINT_BYTE_LEN: usize = FINGERPRINT_HEX_LEN / 2;

/// Non-secret snapshot of encryption-at-rest state for admin observability
/// (cycle /144 — ADR-013 § Decision 8). Carries no raw key or DEK material:
/// only the irreversible key fingerprint and a count of wrapped-DEK rows. Safe
/// to log and expose to admins.
#[derive(Debug, Clone, Serialize)]
pub struct EncryptionStatus {
    /// Whether encryption is active (`false` → `NoopProvider`, full plaintext).
    pub enabled: bool,
    /// Provider discriminant: `"master_key_env"` or `"noop"`.
    pub provider: &'static str,
    /// Master key generation. `None` when disabled.
    pub master_key_version: Option<u8>,
    /// Non-secret key fingerprint (8 hex chars). `None` when disabled.
    pub master_key_fingerprint: Option<String>,
    /// Number of wrapped-DEK rows in the `keys` CF. `None` when disabled.
    pub dek_count: Option<usize>,
}

impl EncryptionStatus {
    /// One-line startup summary (/157 S3, AC-5). Renders only the non-secret
    /// snapshot fields — the fingerprint is the irreversible 8-hex digest.
    pub fn startup_line(&self) -> String {
        format!(
            "Encryption at-rest: enabled={} provider={} key_version={} fingerprint={} dek_count={}",
            self.enabled,
            self.provider,
            self.master_key_version
                .map_or_else(|| "-".to_string(), |v| v.to_string()),
            self.master_key_fingerprint.as_deref().unwrap_or("-"),
            self.dek_count
                .map_or_else(|| "-".to_string(), |c| c.to_string()),
        )
    }
}

/// Storage seam for encryption-at-rest. Held by `RocksDbStore`; the four
/// methods are the entire contract. Construction/IO failures surface as
/// `CryptoError::{Encryption,Decryption}Failed` because the trait is fixed to
/// that error type — operators distinguish causes via `tracing::error`.
pub trait EncryptionProvider: Send + Sync {
    /// Encrypt `plaintext` under the DEK for `scope`, lazily creating it.
    fn encrypt(&self, scope: &str, plaintext: &[u8]) -> Result<Vec<u8>, CryptoError>;
    /// Decrypt `blob` under the DEK for `scope`.
    fn decrypt(&self, scope: &str, blob: &[u8]) -> Result<Vec<u8>, CryptoError>;
    /// Whether encryption is active (false → pass-through `NoopProvider`).
    fn is_enabled(&self) -> bool;
    /// Deterministic, per-scope index token for a name/alias lookup key
    /// (case-insensitive). Lowercasing happens inside this method — callers
    /// pass the original name; the provider normalises before computing the MAC.
    ///
    /// `NoopProvider` returns `plaintext.to_lowercase()` (identity — byte-
    /// identical to today's index keys when encryption is disabled).
    ///
    /// `MasterKeyEnvProvider` returns `hex(HMAC-SHA256(mac_key, lowercase))`
    /// where `mac_key = HMAC-SHA256(stream_dek, b"loomem-index-v1")[..32]`.
    /// ADR-013 §4 Decision 1 — HMAC-SHA256, per-stream key, domain-separated.
    fn index_token(&self, scope: &str, plaintext: &str) -> Result<String, CryptoError>;

    /// Non-secret snapshot of encryption state for admin observability
    /// (cycle /144). Returns no key/DEK material — see [`EncryptionStatus`].
    fn status(&self) -> EncryptionStatus;
}

/// Pass-through provider: returns inputs unchanged, reports disabled.
pub struct NoopProvider;

impl EncryptionProvider for NoopProvider {
    fn encrypt(&self, _scope: &str, plaintext: &[u8]) -> Result<Vec<u8>, CryptoError> {
        Ok(plaintext.to_vec())
    }

    fn decrypt(&self, _scope: &str, blob: &[u8]) -> Result<Vec<u8>, CryptoError> {
        Ok(blob.to_vec())
    }

    fn is_enabled(&self) -> bool {
        false
    }

    /// Identity: returns `plaintext.to_lowercase()` — byte-identical to the
    /// pre-§E index keys. Re-key migration is a no-op under NoopProvider.
    fn index_token(&self, _scope: &str, plaintext: &str) -> Result<String, CryptoError> {
        Ok(plaintext.to_lowercase())
    }

    /// Disabled: no key, no version, no DEK rows.
    fn status(&self) -> EncryptionStatus {
        EncryptionStatus {
            enabled: false,
            provider: "noop",
            master_key_version: None,
            master_key_fingerprint: None,
            dek_count: None,
        }
    }
}

/// FIFO-evicting DEK cache keyed by scope. `(dek, dek_id)` is `Copy`.
#[derive(Default)]
struct DekCache {
    map: HashMap<String, ([u8; 32], u32)>,
    order: VecDeque<String>,
}

/// Real envelope-encryption provider. See module docs.
pub struct MasterKeyEnvProvider {
    master_key: [u8; 32],
    db: Arc<DB>,
    cache: Mutex<DekCache>,
    /// Serializes the lazy-generation slow path so a cold scope hit by N
    /// concurrent writers produces exactly one `keys` row (brief §B AC-B4).
    create_lock: Mutex<()>,
}

impl MasterKeyEnvProvider {
    /// Construct with an already-decoded 32-byte master key and a shared DB
    /// handle (clone of `RocksDbStore::db_arc()`).
    #[must_use]
    pub fn new(master_key: [u8; 32], db: Arc<DB>) -> Self {
        Self {
            master_key,
            db,
            cache: Mutex::new(DekCache::default()),
            create_lock: Mutex::new(()),
        }
    }

    /// Build from `LOOMEM_AT_REST_MASTER_KEY` if set. `Ok(None)` when the var is
    /// absent (caller falls back to `NoopProvider`).
    pub fn from_env(db: Arc<DB>) -> anyhow::Result<Option<Self>> {
        match std::env::var(MASTER_KEY_ENV) {
            Ok(b64) => {
                let key = decode_master_key(&b64)?;
                Ok(Some(Self::new(key, db)))
            }
            Err(_) => Ok(None),
        }
    }

    /// Clear the entire DEK cache. Called by the rotation tool (§G); next
    /// `encrypt`/`decrypt` reloads wrapped DEKs from the `keys` CF.
    pub fn flush_cache(&self) {
        let mut cache = self.cache.lock().unwrap_or_else(PoisonError::into_inner);
        cache.map.clear();
        cache.order.clear();
    }

    /// Non-secret key fingerprint (cycle /144): the first 8 hex chars of
    /// `HMAC-SHA256(master_key, FINGERPRINT_TAG)`. One-way and domain-separated
    /// — safe to log and expose in admin health. Lets an operator compare a
    /// live key against a sealed escrow copy (cycle /145) without revealing
    /// either key.
    #[must_use]
    pub fn fingerprint(&self) -> String {
        fingerprint_key(&self.master_key)
    }

    /// Count wrapped-DEK rows in the `keys` CF (one per scope). Returns 0 when
    /// the CF is absent. Used by [`EncryptionProvider::status`].
    fn dek_count(&self) -> usize {
        let Some(cf) = self.db.cf_handle(CF_KEYS) else {
            return 0;
        };
        self.db
            .iterator_cf(&cf, IteratorMode::Start)
            .filter(|r| {
                if let Err(e) = r {
                    // Surface storage trouble rather than silently under-count:
                    // dek_count feeds escrow verification (ADR-013 §8 / /145),
                    // where a wrong count gives a false match/diverge result.
                    tracing::warn!(error = %e, "dek_count: keys CF iterator error, row skipped");
                    return false;
                }
                true
            })
            .count()
    }

    fn cache_get(&self, scope: &str) -> Option<([u8; 32], u32)> {
        let cache = self.cache.lock().unwrap_or_else(PoisonError::into_inner);
        cache.map.get(scope).copied()
    }

    fn cache_put(&self, scope: &str, dek: [u8; 32], dek_id: u32) {
        let mut cache = self.cache.lock().unwrap_or_else(PoisonError::into_inner);
        if cache.map.contains_key(scope) {
            return;
        }
        while cache.map.len() >= CACHE_CAP {
            match cache.order.pop_front() {
                Some(evicted) => {
                    cache.map.remove(&evicted);
                }
                None => break,
            }
        }
        cache.map.insert(scope.to_string(), (dek, dek_id));
        cache.order.push_back(scope.to_string());
    }

    /// Resolve an existing DEK: cache → `keys` CF. Returns `Ok(None)` when no
    /// row exists. Never writes — this is the read half shared by both paths.
    fn load_dek(&self, scope: &str) -> Result<Option<([u8; 32], u32)>, CryptoError> {
        if let Some(hit) = self.cache_get(scope) {
            return Ok(Some(hit));
        }

        let cf = self.db.cf_handle(CF_KEYS).ok_or_else(|| {
            tracing::error!("keys column family missing; cannot resolve DEK");
            CryptoError::DecryptionFailed
        })?;
        let existing = self
            .db
            .get_cf(&cf, dek_row_key(scope).as_bytes())
            .map_err(|e| {
                tracing::error!(error = %e, scope, "failed reading wrapped DEK from keys CF");
                CryptoError::DecryptionFailed
            })?;
        let Some(bytes) = existing else {
            return Ok(None);
        };

        let wrapped: WrappedStreamDek = bincode::deserialize(&bytes).map_err(|e| {
            tracing::error!(error = %e, scope, "failed deserializing wrapped DEK");
            CryptoError::DecryptionFailed
        })?;
        let dek = at_rest::unwrap_dek(&self.master_key, &wrapped)?;
        self.cache_put(scope, dek, wrapped.dek_id);
        Ok(Some((dek, wrapped.dek_id)))
    }

    /// Resolve the DEK for a decrypt. Errors when the row is absent instead of
    /// minting a fresh (useless) DEK that would persist a stale row and mask
    /// the real cause of the missing key.
    fn get_dek_for_decrypt(&self, scope: &str) -> Result<([u8; 32], u32), CryptoError> {
        self.load_dek(scope)?.ok_or_else(|| {
            tracing::error!(
                scope,
                "no wrapped DEK for scope on decrypt; refusing to generate"
            );
            CryptoError::DecryptionFailed
        })
    }

    /// Resolve the DEK for an encrypt: load → lazily generate+wrap on absence.
    /// Generation is serialized by `create_lock` with a double-checked load,
    /// so concurrent cold writers create exactly one row.
    fn get_or_create_dek(&self, scope: &str) -> Result<([u8; 32], u32), CryptoError> {
        if let Some(hit) = self.load_dek(scope)? {
            return Ok(hit);
        }

        let _guard = self
            .create_lock
            .lock()
            .unwrap_or_else(PoisonError::into_inner);

        // Double-check: a racing writer may have created the row.
        if let Some(hit) = self.load_dek(scope)? {
            return Ok(hit);
        }

        // Absent: generate, wrap, persist. Single-writer under `create_lock`.
        let cf = self.db.cf_handle(CF_KEYS).ok_or_else(|| {
            tracing::error!("keys column family missing; cannot create DEK");
            CryptoError::EncryptionFailed
        })?;
        let dek = at_rest::generate_dek();
        let wrapped =
            at_rest::wrap_dek(&self.master_key, &dek, INITIAL_DEK_ID, MASTER_KEY_VERSION)?;
        let serialized = bincode::serialize(&wrapped).map_err(|e| {
            tracing::error!(error = %e, scope, "failed serializing wrapped DEK");
            CryptoError::EncryptionFailed
        })?;
        self.db
            .put_cf(&cf, dek_row_key(scope).as_bytes(), &serialized)
            .map_err(|e| {
                tracing::error!(error = %e, scope, "failed writing wrapped DEK to keys CF");
                CryptoError::EncryptionFailed
            })?;
        self.cache_put(scope, dek, INITIAL_DEK_ID);
        Ok((dek, INITIAL_DEK_ID))
    }
}

impl EncryptionProvider for MasterKeyEnvProvider {
    fn encrypt(&self, scope: &str, plaintext: &[u8]) -> Result<Vec<u8>, CryptoError> {
        let (dek, dek_id) = self.get_or_create_dek(scope)?;
        at_rest::encrypt_blob(&dek, dek_id, plaintext)
    }

    fn decrypt(&self, scope: &str, blob: &[u8]) -> Result<Vec<u8>, CryptoError> {
        let (dek, _dek_id) = self.get_dek_for_decrypt(scope)?;
        at_rest::decrypt_blob(&dek, blob)
    }

    fn is_enabled(&self) -> bool {
        true
    }

    /// Compute a per-stream HMAC-SHA256 index token (ADR-013 §4 Decision 1 + 4).
    ///
    /// Derivation:
    ///   mac_key = HMAC-SHA256(stream_dek, b"loomem-index-v1")[..32]
    ///   token   = hex(HMAC-SHA256(mac_key, lowercase(plaintext)))
    ///
    /// Lowercasing uses `str::to_lowercase()` (full Unicode, same as the
    /// pre-§E site-level `.to_lowercase()` calls — parity verified: same
    /// Unicode lowercasing algorithm, no substitution with `to_ascii_lowercase`).
    /// The raw DEK is not exposed; derivation is domain-separated with tag
    /// "loomem-index-v1" so the mac_key is unlinkable to the encryption DEK.
    fn index_token(&self, scope: &str, plaintext: &str) -> Result<String, CryptoError> {
        let (dek, _dek_id) = self.get_or_create_dek(scope)?;
        let mac_key = derive_mac_key(&dek);
        Ok(at_rest::index_token(
            &mac_key,
            plaintext.to_lowercase().as_bytes(),
        ))
    }

    /// Enabled: reports version, non-secret fingerprint, and wrapped-DEK count.
    fn status(&self) -> EncryptionStatus {
        EncryptionStatus {
            enabled: true,
            provider: "master_key_env",
            master_key_version: Some(MASTER_KEY_VERSION),
            master_key_fingerprint: Some(self.fingerprint()),
            dek_count: Some(self.dek_count()),
        }
    }
}

/// Derive a per-stream MAC key from the stream DEK, domain-separated with the
/// constant tag `"loomem-index-v1"`. Returns the first 32 bytes of the
/// HMAC-SHA256 output (all 32 bytes — SHA-256 output is 32 bytes).
///
/// ADR-013 §4 Decision 4: MAC key derived from `stream_dek` inside the
/// provider; raw DEK never exposed to callers.
fn derive_mac_key(dek: &[u8; 32]) -> [u8; 32] {
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(dek)
        .expect("HMAC-SHA256 accepts any key length; 32-byte DEK is always valid");
    mac.update(b"loomem-index-v1");
    let result = mac.finalize().into_bytes();
    let mut out = [0u8; 32];
    out.copy_from_slice(&result);
    out
}

/// Compute the non-secret key fingerprint (cycle /144): the first
/// [`FINGERPRINT_HEX_LEN`] hex chars of `HMAC-SHA256(master_key, FINGERPRINT_TAG)`.
///
/// One-way (keyed MAC + truncation) and domain-separated from both the
/// encryption path and the `"loomem-index-v1"` index MAC-key derivation, so the
/// output is unlinkable to any DEK or index token. Free function (not a method)
/// so it can be unit-tested against a known-key → known-vector pair.
fn fingerprint_key(master_key: &[u8; 32]) -> String {
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(master_key)
        .expect("HMAC-SHA256 accepts any key length; 32-byte key is always valid");
    mac.update(FINGERPRINT_TAG);
    let result = mac.finalize().into_bytes();
    result.iter().take(FINGERPRINT_BYTE_LEN).fold(
        String::with_capacity(FINGERPRINT_HEX_LEN),
        |mut s, b| {
            use std::fmt::Write as _;
            let _ = write!(s, "{b:02x}");
            s
        },
    )
}

/// Row key for a scope's wrapped DEK within the `keys` CF.
fn dek_row_key(scope: &str) -> String {
    format!("scope:{scope}")
}

/// Decode the base64 master key into a 32-byte array.
///
/// Hand-rolled standard (RFC 4648) decode because the workspace deliberately
/// avoids a `base64` crate (cf. `loomem-server` `decode_base64_chunk`); adding
/// a dependency is out of scope for §B. Consolidating the repo's three
/// hand-rolled decoders into one util is a follow-up.
pub fn decode_master_key(b64: &str) -> anyhow::Result<[u8; 32]> {
    let raw = decode_base64_standard(b64)
        .map_err(|e| anyhow::anyhow!("{MASTER_KEY_ENV} base64 decode failed: {e}"))?;
    <[u8; 32]>::try_from(raw.as_slice()).map_err(|_| {
        anyhow::anyhow!(
            "{MASTER_KEY_ENV} must decode to exactly 32 bytes, got {}",
            raw.len()
        )
    })
}

/// Base64 (RFC 4648) decode with optional `=` padding, accepting both the
/// standard and URL-safe alphabets (see `b64_sextet`). Whitespace is trimmed at
/// the ends; interior invalid characters are rejected.
fn decode_base64_standard(input: &str) -> Result<Vec<u8>, String> {
    let bytes = input.trim().as_bytes();
    let mut end = bytes.len();
    while end > 0 && bytes[end - 1] == b'=' {
        end -= 1;
    }
    let data = &bytes[..end];

    let mut out = Vec::with_capacity(data.len() * 3 / 4);
    let mut acc: u32 = 0;
    let mut nbits: u32 = 0;
    for &c in data {
        let value = b64_sextet(c).ok_or_else(|| format!("invalid base64 byte {c:#04x}"))?;
        acc = (acc << 6) | u32::from(value);
        nbits += 6;
        if nbits >= 8 {
            nbits -= 8;
            let byte = u8::try_from((acc >> nbits) & 0xFF)
                .map_err(|_| "base64 byte overflow".to_string())?;
            out.push(byte);
        }
    }
    Ok(out)
}

/// Map one base64 character to its 6-bit value, or `None` if not in the alphabet.
///
/// Accepts both the standard (`+`/`/`) and URL-safe (`-`/`_`) alphabets; the two
/// never collide, so common url-safe key generators (`secrets.token_urlsafe`,
/// `randomBytes().toString('base64url')`) decode without extra operator effort.
fn b64_sextet(c: u8) -> Option<u8> {
    match c {
        b'A'..=b'Z' => Some(c - b'A'),
        b'a'..=b'z' => Some(c - b'a' + 26),
        b'0'..=b'9' => Some(c - b'0' + 52),
        b'+' | b'-' => Some(62),
        b'/' | b'_' => Some(63),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{RocksDbConfig, RocksDbStore};
    use rocksdb::IteratorMode;
    use tempfile::TempDir;

    fn rocks_cfg() -> RocksDbConfig {
        RocksDbConfig {
            max_open_files: 100,
            compression: "lz4".to_string(),
            write_buffer_size: 4 * 1024 * 1024,
            max_write_buffer_number: 2,
        }
    }

    fn test_store() -> (TempDir, Arc<RocksDbStore>) {
        let tmp = TempDir::new().expect("tempdir");
        let store = Arc::new(RocksDbStore::open(tmp.path(), &rocks_cfg()).expect("open store"));
        (tmp, store)
    }

    fn keys_row_count(store: &RocksDbStore) -> usize {
        let cf = store.db().cf_handle(CF_KEYS).expect("keys cf");
        store
            .db()
            .iterator_cf(&cf, IteratorMode::Start)
            .filter(|r| r.is_ok())
            .count()
    }

    // AC-B6 #1: round-trip per scope across three representative scopes.
    #[test]
    fn master_key_provider_roundtrip_per_scope() {
        let (_tmp, store) = test_store();
        let provider = MasterKeyEnvProvider::new([7u8; 32], store.db_arc());
        assert!(provider.is_enabled());
        for scope in ["__shared_team__", "team-test-user", "__project_test__"] {
            let plaintext = format!("secret payload for {scope}").into_bytes();
            let blob = provider.encrypt(scope, &plaintext).expect("encrypt");
            assert!(at_rest::is_encrypted(&blob));
            let recovered = provider.decrypt(scope, &blob).expect("decrypt");
            assert_eq!(
                recovered, plaintext,
                "round-trip identity for scope {scope}"
            );
        }
    }

    // AC-B6 #2: NoopProvider reports disabled and passes through unchanged.
    #[test]
    fn noop_provider_passthrough_and_disabled() {
        let provider = NoopProvider;
        assert!(!provider.is_enabled());
        let input = b"plaintext bytes";
        assert_eq!(provider.encrypt("any", input).expect("encrypt"), input);
        assert_eq!(provider.decrypt("any", input).expect("decrypt"), input);
    }

    // AC-B6 #3: first encrypt for a scope creates exactly one keys CF row.
    #[test]
    fn lazy_gen_creates_keys_row() {
        let (_tmp, store) = test_store();
        let provider = MasterKeyEnvProvider::new([3u8; 32], store.db_arc());
        let cf = store.db().cf_handle(CF_KEYS).expect("keys cf");
        assert!(
            store
                .db()
                .get_cf(&cf, b"scope:scope_x")
                .expect("get")
                .is_none(),
            "no row before first encrypt"
        );
        let _ = provider.encrypt("scope_x", b"data").expect("encrypt");
        let row = store
            .db()
            .get_cf(&cf, b"scope:scope_x")
            .expect("get")
            .expect("row present after first encrypt");
        assert!(!row.is_empty());
        assert_eq!(keys_row_count(&store), 1);
    }

    // AC-B6 #4: 100 concurrent first-writes for the same scope → exactly 1 row.
    #[test]
    fn concurrent_first_write_creates_exactly_one_row() {
        let (_tmp, store) = test_store();
        let provider = Arc::new(MasterKeyEnvProvider::new([9u8; 32], store.db_arc()));
        std::thread::scope(|s| {
            for _ in 0..100 {
                let p = Arc::clone(&provider);
                s.spawn(move || {
                    let _ = p
                        .encrypt("same_scope", b"x")
                        .expect("encrypt under contention");
                });
            }
        });
        assert_eq!(
            keys_row_count(&store),
            1,
            "concurrent cold writers must create exactly one DEK row"
        );
        // Sanity: the single shared DEK still round-trips.
        let blob = provider.encrypt("same_scope", b"after").expect("encrypt");
        assert_eq!(
            provider.decrypt("same_scope", &blob).expect("decrypt"),
            b"after"
        );
    }

    // AC-B6 #5: decrypting with a different master key (rotation simulation) fails.
    #[test]
    fn wrong_master_key_fails_decrypt() {
        let (_tmp, store) = test_store();
        let provider_a = MasterKeyEnvProvider::new([1u8; 32], store.db_arc());
        let blob = provider_a
            .encrypt("scope_r", b"rotate me")
            .expect("encrypt with A");
        // Fresh provider, different master key, same DB (and thus same wrapped DEK row).
        let provider_b = MasterKeyEnvProvider::new([2u8; 32], store.db_arc());
        match provider_b.decrypt("scope_r", &blob) {
            Err(CryptoError::DecryptionFailed) => {}
            other => panic!("expected DecryptionFailed, got {other:?}"),
        }
    }

    // AC-E1: NoopProvider index_token returns plaintext.to_lowercase() (identity).
    #[test]
    fn noop_index_token_identity() {
        let p = NoopProvider;
        assert_eq!(p.index_token("any", "Alice").unwrap(), "alice");
        assert_eq!(p.index_token("any", "ANNA").unwrap(), "anna");
        assert_eq!(p.index_token("any", "alice").unwrap(), "alice");
    }

    // AC-E1: MasterKeyEnvProvider index_token — determinism (same in → same out).
    #[test]
    fn master_key_index_token_deterministic() {
        let (_tmp, store) = test_store();
        let p = MasterKeyEnvProvider::new([5u8; 32], store.db_arc());
        let t1 = p.index_token("stream-a", "Alice Smith").unwrap();
        let t2 = p.index_token("stream-a", "Alice Smith").unwrap();
        assert_eq!(t1, t2, "index_token must be deterministic");
    }

    // AC-E1: case-insensitivity — "Alice" and "alice" produce the same token.
    #[test]
    fn master_key_index_token_case_insensitive() {
        let (_tmp, store) = test_store();
        let p = MasterKeyEnvProvider::new([5u8; 32], store.db_arc());
        let t_upper = p.index_token("stream-a", "Alice Smith").unwrap();
        let t_lower = p.index_token("stream-a", "alice smith").unwrap();
        assert_eq!(t_upper, t_lower, "tokens must match regardless of case");
    }

    // AC-E1: Unicode case-insensitivity (Anna / anna → same token).
    #[test]
    fn master_key_index_token_unicode_case_insensitive() {
        let (_tmp, store) = test_store();
        let p = MasterKeyEnvProvider::new([5u8; 32], store.db_arc());
        let t_mixed = p.index_token("stream-a", "Anna").unwrap();
        let t_lower = p.index_token("stream-a", "anna").unwrap();
        assert_eq!(t_mixed, t_lower, "Unicode lowercase parity: Anna == anna");
        // Parity check: str::to_lowercase semantics preserved (not ASCII-only).
        assert_eq!("Anna".to_lowercase(), "anna");
    }

    // AC-E1: per-scope divergence — same name, different scope → different token.
    #[test]
    fn master_key_index_token_per_scope_divergence() {
        let (_tmp, store) = test_store();
        let p = MasterKeyEnvProvider::new([5u8; 32], store.db_arc());
        let t_a = p.index_token("scope-A", "alice").unwrap();
        let t_b = p.index_token("scope-B", "alice").unwrap();
        assert_ne!(
            t_a, t_b,
            "same name in different scopes must produce different tokens (per-stream MAC key)"
        );
    }

    // AC-E1: output is 64 lowercase hex chars (HMAC-SHA256 = 32 bytes → 64 hex).
    #[test]
    fn master_key_index_token_output_format() {
        let (_tmp, store) = test_store();
        let p = MasterKeyEnvProvider::new([5u8; 32], store.db_arc());
        let t = p.index_token("scope-x", "Bob").unwrap();
        assert_eq!(t.len(), 64, "HMAC-SHA256 hex output must be 64 chars");
        assert!(
            t.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase()),
            "output must be lowercase hex, got: {t}"
        );
    }

    // Regression: decrypt on a scope with no keys-CF row must error without
    // generating/persisting a fresh DEK (no stale write, no orphan row).
    #[test]
    fn decrypt_missing_row_errors_without_write() {
        let (_tmp, store) = test_store();
        let provider = MasterKeyEnvProvider::new([5u8; 32], store.db_arc());
        match provider.decrypt("never_encrypted", b"\x00garbage") {
            Err(CryptoError::DecryptionFailed) => {}
            other => panic!("expected DecryptionFailed, got {other:?}"),
        }
        assert_eq!(
            keys_row_count(&store),
            0,
            "decrypt must not create a DEK row for a missing scope"
        );
    }

    // Extra: validate the hand-rolled base64 decoder against known vectors.
    #[test]
    fn decode_base64_known_vectors() {
        assert_eq!(decode_base64_standard("TWFu").expect("Man"), b"Man");
        assert_eq!(decode_base64_standard("TWE=").expect("Ma"), b"Ma");
        assert_eq!(decode_base64_standard("TQ==").expect("M"), b"M");
        assert!(decode_base64_standard("****").is_err());
        // URL-safe alphabet decodes identically to its standard counterpart.
        assert_eq!(
            decode_base64_standard("____").expect("url-safe /"),
            decode_base64_standard("////").expect("standard /")
        );
        assert_eq!(
            decode_base64_standard("----").expect("url-safe +"),
            decode_base64_standard("++++").expect("standard +")
        );
    }

    // Extra: master-key decode accepts a 32-byte payload, rejects wrong length.
    #[test]
    fn decode_master_key_length_check() {
        let all_zero_b64 = format!("{}=", "A".repeat(43)); // 32 zero bytes
        assert_eq!(
            decode_master_key(&all_zero_b64).expect("32 bytes"),
            [0u8; 32]
        );
        assert!(decode_master_key("TWFu").is_err()); // 3 bytes
    }

    // AC-4 (/144): fingerprint matches a pinned known-key → known-vector,
    // confirming the exact derivation `hex(HMAC-SHA256(key, tag))[..8]`.
    #[test]
    fn fingerprint_known_vector() {
        // hmac.new([7;32], b"loomem-key-fingerprint-v1", sha256).hexdigest()[:8]
        assert_eq!(fingerprint_key(&[7u8; 32]), "1f7e91cc");
        let (_tmp, store) = test_store();
        let provider = MasterKeyEnvProvider::new([7u8; 32], store.db_arc());
        assert_eq!(provider.fingerprint(), "1f7e91cc");
    }

    // AC-4 (/144): deterministic, differs per key, 8 lowercase hex chars.
    #[test]
    fn fingerprint_deterministic_and_distinct() {
        assert_eq!(fingerprint_key(&[1u8; 32]), fingerprint_key(&[1u8; 32]));
        assert_eq!(fingerprint_key(&[1u8; 32]), "d1ff59a3");
        assert_eq!(fingerprint_key(&[2u8; 32]), "da3b9737");
        assert_ne!(
            fingerprint_key(&[1u8; 32]),
            fingerprint_key(&[2u8; 32]),
            "distinct keys must produce distinct fingerprints"
        );
        let fp = fingerprint_key(&[1u8; 32]);
        assert_eq!(fp.len(), FINGERPRINT_HEX_LEN);
        assert!(
            fp.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase()),
            "fingerprint must be lowercase hex, got: {fp}"
        );
    }

    // AC-5 (/144): NoopProvider status reports disabled with no key material.
    #[test]
    fn status_noop_disabled_no_material() {
        let s = NoopProvider.status();
        assert!(!s.enabled);
        assert_eq!(s.provider, "noop");
        assert_eq!(s.master_key_version, None);
        assert_eq!(s.master_key_fingerprint, None);
        assert_eq!(s.dek_count, None);
    }

    // AC-2/AC-5 (/144): MasterKeyEnvProvider status — enabled, fingerprint,
    // version, and a dek_count that tracks wrapped-DEK rows (0 → 1 after a write).
    #[test]
    fn status_master_key_reports_fingerprint_and_dek_count() {
        let (_tmp, store) = test_store();
        let provider = MasterKeyEnvProvider::new([7u8; 32], store.db_arc());

        let before = provider.status();
        assert!(before.enabled);
        assert_eq!(before.provider, "master_key_env");
        assert_eq!(before.master_key_version, Some(MASTER_KEY_VERSION));
        assert_eq!(before.master_key_fingerprint.as_deref(), Some("1f7e91cc"));
        assert_eq!(
            before.dek_count,
            Some(0),
            "no DEK rows before first encrypt"
        );

        let _ = provider.encrypt("scope_a", b"data").expect("encrypt");
        assert_eq!(
            provider.status().dek_count,
            Some(1),
            "one DEK row after first encrypt"
        );
    }

    /// AC-5 (/157): startup line golden format for an enabled provider —
    /// exactly the snapshot fields, nothing secret-shaped beyond the 8-hex
    /// fingerprint.
    #[test]
    fn startup_line_enabled_golden() {
        let s = EncryptionStatus {
            enabled: true,
            provider: "master_key_env",
            master_key_version: Some(1),
            master_key_fingerprint: Some("48a84d7a".to_string()),
            dek_count: Some(22),
        };
        assert_eq!(
            s.startup_line(),
            "Encryption at-rest: enabled=true provider=master_key_env key_version=1 fingerprint=48a84d7a dek_count=22"
        );
    }

    /// AC-5 (/157): disabled provider renders dashes for every key-derived
    /// field.
    #[test]
    fn startup_line_disabled_golden() {
        assert_eq!(
            NoopProvider.status().startup_line(),
            "Encryption at-rest: enabled=false provider=noop key_version=- fingerprint=- dek_count=-"
        );
    }
}
