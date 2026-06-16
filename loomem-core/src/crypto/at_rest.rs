// AES-256-GCM envelope encryption for row values at rest.
// Format and rationale: docs/decisions/ADR-013-encryption-at-rest.md §6.
//
// §E: also contains `index_token` — a low-level HMAC-SHA256 helper for
// deriving deterministic, opaque RocksDB key suffixes from plaintext names.
// Rationale: ADR-013 §4 "Reverse-name index" (amended 2026-06-01).

use aes_gcm::aead::{Aead, KeyInit, OsRng};
use aes_gcm::{AeadCore, Aes256Gcm, Key, Nonce};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::time::{SystemTime, UNIX_EPOCH};
use thiserror::Error;

pub const MAGIC: [u8; 4] = [0xFF, 0x4C, 0x4F, 0x4F];
pub const ENCRYPTION_VERSION_V1: u8 = 1;
const NONCE_SIZE: usize = 12;
const TAG_SIZE: usize = 16;
const DEK_SIZE: usize = 32;
const HEADER_SIZE: usize = MAGIC.len() + 1 + 4 + NONCE_SIZE;
pub const MIN_ENCRYPTED_BLOB_SIZE: usize = HEADER_SIZE + TAG_SIZE;

#[derive(Debug, Error)]
pub enum CryptoError {
    #[error("legacy plaintext blob (magic absent)")]
    LegacyPlaintext,
    #[error("invalid magic bytes")]
    InvalidMagic,
    #[error("blob too short: {0} bytes, minimum {1}")]
    BlobTooShort(usize, usize),
    #[error("unsupported encryption version: {0}")]
    UnsupportedVersion(u8),
    #[error("AEAD decryption failed (wrong key or tampered ciphertext)")]
    DecryptionFailed,
    #[error("AEAD encryption failed")]
    EncryptionFailed,
    #[error("unwrapped DEK size mismatch: got {0}, expected {1}")]
    KeySizeMismatch(usize, usize),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WrappedStreamDek {
    pub dek_id: u32,
    pub master_key_version: u8,
    pub wrapped_blob: Vec<u8>,
    pub wrap_nonce: [u8; NONCE_SIZE],
    pub wrap_tag: [u8; TAG_SIZE],
    pub created_at: i64,
}

#[inline]
#[must_use]
pub fn is_encrypted(blob: &[u8]) -> bool {
    blob.len() > MAGIC.len() && blob[0..MAGIC.len()] == MAGIC
}

pub fn read_dek_id(blob: &[u8]) -> Result<u32, CryptoError> {
    if blob.len() < HEADER_SIZE {
        return Err(CryptoError::BlobTooShort(
            blob.len(),
            MIN_ENCRYPTED_BLOB_SIZE,
        ));
    }
    if blob[0..MAGIC.len()] != MAGIC {
        return Err(CryptoError::InvalidMagic);
    }
    let dek_id_start = MAGIC.len() + 1;
    let dek_id_end = dek_id_start + 4;
    let mut bytes = [0u8; 4];
    bytes.copy_from_slice(&blob[dek_id_start..dek_id_end]);
    Ok(u32::from_le_bytes(bytes))
}

pub fn encrypt_blob(
    dek: &[u8; DEK_SIZE],
    dek_id: u32,
    plaintext: &[u8],
) -> Result<Vec<u8>, CryptoError> {
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(dek));
    let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
    let ct_and_tag = cipher
        .encrypt(&nonce, plaintext)
        .map_err(|_| CryptoError::EncryptionFailed)?;

    let mut out = Vec::with_capacity(HEADER_SIZE + ct_and_tag.len());
    out.extend_from_slice(&MAGIC);
    out.push(ENCRYPTION_VERSION_V1);
    out.extend_from_slice(&dek_id.to_le_bytes());
    out.extend_from_slice(nonce.as_slice());
    out.extend_from_slice(&ct_and_tag);
    Ok(out)
}

pub fn decrypt_blob(dek: &[u8; DEK_SIZE], blob: &[u8]) -> Result<Vec<u8>, CryptoError> {
    if !is_encrypted(blob) {
        return Err(CryptoError::LegacyPlaintext);
    }
    if blob.len() < MIN_ENCRYPTED_BLOB_SIZE {
        return Err(CryptoError::BlobTooShort(
            blob.len(),
            MIN_ENCRYPTED_BLOB_SIZE,
        ));
    }
    let version = blob[MAGIC.len()];
    if version != ENCRYPTION_VERSION_V1 {
        return Err(CryptoError::UnsupportedVersion(version));
    }
    let nonce_start = MAGIC.len() + 1 + 4;
    let nonce_end = nonce_start + NONCE_SIZE;
    let nonce = Nonce::from_slice(&blob[nonce_start..nonce_end]);
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(dek));
    cipher
        .decrypt(nonce, &blob[nonce_end..])
        .map_err(|_| CryptoError::DecryptionFailed)
}

#[must_use]
pub fn generate_dek() -> [u8; DEK_SIZE] {
    let key = Aes256Gcm::generate_key(&mut OsRng);
    let mut out = [0u8; DEK_SIZE];
    out.copy_from_slice(key.as_slice());
    out
}

pub fn wrap_dek(
    master_key: &[u8; DEK_SIZE],
    dek: &[u8; DEK_SIZE],
    dek_id: u32,
    master_key_version: u8,
) -> Result<WrappedStreamDek, CryptoError> {
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(master_key));
    let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
    let ct_and_tag = cipher
        .encrypt(&nonce, dek.as_slice())
        .map_err(|_| CryptoError::EncryptionFailed)?;

    // aes-gcm appends the 16-byte tag to ciphertext. Split into stored fields.
    if ct_and_tag.len() != DEK_SIZE + TAG_SIZE {
        return Err(CryptoError::EncryptionFailed);
    }
    let mut wrap_nonce = [0u8; NONCE_SIZE];
    wrap_nonce.copy_from_slice(nonce.as_slice());
    let mut wrap_tag = [0u8; TAG_SIZE];
    wrap_tag.copy_from_slice(&ct_and_tag[DEK_SIZE..]);
    let wrapped_blob = ct_and_tag[..DEK_SIZE].to_vec();

    let created_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0);

    Ok(WrappedStreamDek {
        dek_id,
        master_key_version,
        wrapped_blob,
        wrap_nonce,
        wrap_tag,
        created_at,
    })
}

/// Compute a deterministic, opaque index token for a reverse-name index key.
///
/// Returns `lowercase-hex(HMAC-SHA256(mac_key, lowercased_bytes))`. The
/// `lowercased` slice MUST already be UTF-8 lowercased by the caller (or by
/// `EncryptionProvider::index_token`, which owns normalization). The MAC key is
/// a 32-byte per-stream key derived from the scope's DEK (see provider.rs).
///
/// ADR-013 §4 Decision 1: HMAC-SHA256 (not BLAKE3). Primitive kept here for
/// unit-testability in isolation from the provider layer.
pub fn index_token(mac_key: &[u8; DEK_SIZE], lowercased: &[u8]) -> String {
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(mac_key)
        .expect("HMAC-SHA256 accepts any key length; 32-byte key is always valid");
    mac.update(lowercased);
    let result = mac.finalize().into_bytes();
    // 64 lowercase hex chars (32 bytes × 2).
    result.iter().fold(String::with_capacity(64), |mut s, b| {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
        s
    })
}

pub fn unwrap_dek(
    master_key: &[u8; DEK_SIZE],
    wrapped: &WrappedStreamDek,
) -> Result<[u8; DEK_SIZE], CryptoError> {
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(master_key));
    let nonce = Nonce::from_slice(&wrapped.wrap_nonce);

    let mut ct_and_tag = Vec::with_capacity(wrapped.wrapped_blob.len() + TAG_SIZE);
    ct_and_tag.extend_from_slice(&wrapped.wrapped_blob);
    ct_and_tag.extend_from_slice(&wrapped.wrap_tag);

    let raw = cipher
        .decrypt(nonce, ct_and_tag.as_ref())
        .map_err(|_| CryptoError::DecryptionFailed)?;

    if raw.len() != DEK_SIZE {
        return Err(CryptoError::KeySizeMismatch(raw.len(), DEK_SIZE));
    }
    let mut out = [0u8; DEK_SIZE];
    out.copy_from_slice(&raw);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aes_gcm::aead::rand_core::RngCore;

    fn fixed_dek() -> [u8; DEK_SIZE] {
        let mut k = [0u8; DEK_SIZE];
        for (i, b) in k.iter_mut().enumerate() {
            *b = u8::try_from(i).unwrap_or(0);
        }
        k
    }

    fn random_dek() -> [u8; DEK_SIZE] {
        let mut k = [0u8; DEK_SIZE];
        OsRng.fill_bytes(&mut k);
        k
    }

    #[test]
    fn is_encrypted_recognises_magic_prefix() {
        let mut blob = vec![0xFF, 0x4C, 0x4F, 0x4F, 0x01];
        blob.extend_from_slice(&[0u8; 32]);
        assert!(is_encrypted(&blob));
    }

    #[test]
    fn is_encrypted_rejects_plaintext() {
        assert!(!is_encrypted(b"plaintext"));
        assert!(!is_encrypted(b"{\"json\": 1}"));
        assert!(!is_encrypted(&[0xFF])); // too short
        assert!(!is_encrypted(&[0xFF, 0x4C, 0x4F, 0x46, 0x01])); // magic byte 3 wrong
        assert!(!is_encrypted(&[])); // empty
    }

    #[test]
    fn roundtrip_random_plaintext_100_iterations() {
        let dek = fixed_dek();
        for i in 0..100 {
            let mut plaintext = vec![0u8; 16 + (i * 3) % 200];
            OsRng.fill_bytes(&mut plaintext);
            let blob = encrypt_blob(&dek, 42, &plaintext).expect("encrypt should succeed");
            assert!(is_encrypted(&blob));
            assert_eq!(blob[0..4], MAGIC);
            assert_eq!(blob[4], ENCRYPTION_VERSION_V1);
            let got = decrypt_blob(&dek, &blob).expect("decrypt should succeed");
            assert_eq!(got, plaintext);
        }
    }

    #[test]
    fn empty_plaintext_roundtrip() {
        let dek = fixed_dek();
        let blob = encrypt_blob(&dek, 7, b"").expect("encrypt empty");
        assert_eq!(blob.len(), MIN_ENCRYPTED_BLOB_SIZE);
        let got = decrypt_blob(&dek, &blob).expect("decrypt empty");
        assert!(got.is_empty());
    }

    #[test]
    fn dek_id_round_trips_through_blob() {
        let dek = fixed_dek();
        for dek_id in [0u32, 1, 42, u32::MAX, 0x0102_0304] {
            let blob = encrypt_blob(&dek, dek_id, b"payload").expect("encrypt");
            assert_eq!(read_dek_id(&blob).expect("read_dek_id"), dek_id);
        }
    }

    #[test]
    fn nonce_differs_across_encrypts_of_same_plaintext() {
        let dek = fixed_dek();
        let a = encrypt_blob(&dek, 1, b"identical").expect("a");
        let b = encrypt_blob(&dek, 1, b"identical").expect("b");
        // Header up to nonce is identical; nonce + ciphertext + tag differ.
        let nonce_start = MAGIC.len() + 1 + 4;
        let nonce_end = nonce_start + NONCE_SIZE;
        assert_ne!(a[nonce_start..nonce_end], b[nonce_start..nonce_end]);
        assert_ne!(a[nonce_end..], b[nonce_end..]);
    }

    #[test]
    fn tampered_ciphertext_fails() {
        let dek = fixed_dek();
        let mut blob = encrypt_blob(&dek, 1, b"payload data").expect("encrypt");
        let ct_start = HEADER_SIZE;
        blob[ct_start] ^= 0x01;
        match decrypt_blob(&dek, &blob) {
            Err(CryptoError::DecryptionFailed) => {}
            other => panic!("expected DecryptionFailed, got {other:?}"),
        }
    }

    #[test]
    fn tampered_tag_fails() {
        let dek = fixed_dek();
        let mut blob = encrypt_blob(&dek, 1, b"payload").expect("encrypt");
        let last = blob.len() - 1;
        blob[last] ^= 0x80;
        match decrypt_blob(&dek, &blob) {
            Err(CryptoError::DecryptionFailed) => {}
            other => panic!("expected DecryptionFailed, got {other:?}"),
        }
    }

    #[test]
    fn wrong_key_fails() {
        let blob = encrypt_blob(&fixed_dek(), 1, b"secret").expect("encrypt");
        let mut other = fixed_dek();
        other[0] ^= 0xFF;
        match decrypt_blob(&other, &blob) {
            Err(CryptoError::DecryptionFailed) => {}
            other => panic!("expected DecryptionFailed, got {other:?}"),
        }
    }

    #[test]
    fn decrypt_rejects_legacy_plaintext() {
        let dek = fixed_dek();
        match decrypt_blob(&dek, b"this is plaintext, no magic") {
            Err(CryptoError::LegacyPlaintext) => {}
            other => panic!("expected LegacyPlaintext, got {other:?}"),
        }
    }

    #[test]
    fn decrypt_rejects_short_blob() {
        let dek = fixed_dek();
        // Magic-only blob (4B) is recognised as encrypted by is_encrypted's >= magic+1 rule
        // only when >= 5 bytes — so test against a length below MIN_ENCRYPTED_BLOB_SIZE
        // but with valid magic + version prefix.
        let mut short = Vec::from(MAGIC);
        short.push(ENCRYPTION_VERSION_V1);
        short.extend_from_slice(&[0u8; 10]); // too short for header + tag
        match decrypt_blob(&dek, &short) {
            Err(CryptoError::BlobTooShort(_, _)) => {}
            other => panic!("expected BlobTooShort, got {other:?}"),
        }
    }

    #[test]
    fn decrypt_rejects_unsupported_version() {
        let dek = fixed_dek();
        let mut blob = encrypt_blob(&dek, 1, b"payload").expect("encrypt");
        blob[MAGIC.len()] = 99;
        match decrypt_blob(&dek, &blob) {
            Err(CryptoError::UnsupportedVersion(99)) => {}
            other => panic!("expected UnsupportedVersion(99), got {other:?}"),
        }
    }

    #[test]
    fn read_dek_id_rejects_invalid_magic() {
        let mut blob = vec![0x00, 0x00, 0x00, 0x00, 0x01];
        blob.extend_from_slice(&[0u8; HEADER_SIZE + TAG_SIZE]);
        match read_dek_id(&blob) {
            Err(CryptoError::InvalidMagic) => {}
            other => panic!("expected InvalidMagic, got {other:?}"),
        }
    }

    #[test]
    fn read_dek_id_rejects_short_blob() {
        match read_dek_id(&[0xFF, 0x4C]) {
            Err(CryptoError::BlobTooShort(2, _)) => {}
            other => panic!("expected BlobTooShort, got {other:?}"),
        }
    }

    // AC-E1 (low-level primitive): determinism, length, hex-only output.
    #[test]
    fn index_token_deterministic_and_hex() {
        let mac_key = [0u8; DEK_SIZE];
        let t1 = index_token(&mac_key, b"alice");
        let t2 = index_token(&mac_key, b"alice");
        assert_eq!(t1, t2, "index_token must be deterministic");
        assert_eq!(t1.len(), 64, "HMAC-SHA256 produces 32 bytes = 64 hex chars");
        assert!(
            t1.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase()),
            "output must be lowercase hex"
        );
    }

    // AC-E1 (low-level): different inputs produce different tokens (collision check).
    #[test]
    fn index_token_differs_for_different_inputs() {
        let mac_key = [1u8; DEK_SIZE];
        let t_name = index_token(&mac_key, b"alice smith");
        let t_alias = index_token(&mac_key, b"alice");
        assert_ne!(
            t_name, t_alias,
            "different inputs must produce different tokens"
        );
    }

    // AC-E1 (low-level): different mac keys produce different tokens for same input.
    #[test]
    fn index_token_differs_for_different_keys() {
        let t1 = index_token(&[0u8; DEK_SIZE], b"alice");
        let t2 = index_token(&[1u8; DEK_SIZE], b"alice");
        assert_ne!(t1, t2, "different mac keys must produce different tokens");
    }

    #[test]
    fn generate_dek_returns_unique_keys() {
        let a = generate_dek();
        let b = generate_dek();
        assert_eq!(a.len(), DEK_SIZE);
        assert_ne!(a, b);
        assert_ne!(a, [0u8; DEK_SIZE]);
    }

    #[test]
    fn wrap_unwrap_round_trip() {
        let master = random_dek();
        let dek = random_dek();
        let wrapped = wrap_dek(&master, &dek, 7, 1).expect("wrap");
        assert_eq!(wrapped.dek_id, 7);
        assert_eq!(wrapped.master_key_version, 1);
        assert_eq!(wrapped.wrapped_blob.len(), DEK_SIZE);
        let raw = unwrap_dek(&master, &wrapped).expect("unwrap");
        assert_eq!(raw, dek);
    }

    #[test]
    fn unwrap_with_wrong_master_key_fails() {
        let master = random_dek();
        let dek = random_dek();
        let wrapped = wrap_dek(&master, &dek, 1, 1).expect("wrap");
        let mut other = master;
        other[0] ^= 0xFF;
        match unwrap_dek(&other, &wrapped) {
            Err(CryptoError::DecryptionFailed) => {}
            other => panic!("expected DecryptionFailed, got {other:?}"),
        }
    }

    #[test]
    fn unwrap_detects_tampered_wrap_tag() {
        let master = random_dek();
        let dek = random_dek();
        let mut wrapped = wrap_dek(&master, &dek, 1, 1).expect("wrap");
        wrapped.wrap_tag[0] ^= 0x01;
        match unwrap_dek(&master, &wrapped) {
            Err(CryptoError::DecryptionFailed) => {}
            other => panic!("expected DecryptionFailed, got {other:?}"),
        }
    }
}
