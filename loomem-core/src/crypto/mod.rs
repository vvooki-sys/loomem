// Encryption-at-rest primitives. Authoritative spec: ADR-013.
// §6 blob format: magic || version || dek_id || nonce || ciphertext || tag.

pub mod at_rest;
pub mod provider;

pub use at_rest::{
    decrypt_blob, encrypt_blob, generate_dek, is_encrypted, read_dek_id, unwrap_dek, wrap_dek,
    CryptoError, WrappedStreamDek, ENCRYPTION_VERSION_V1, MAGIC, MIN_ENCRYPTED_BLOB_SIZE,
};
pub use provider::{EncryptionProvider, MasterKeyEnvProvider, NoopProvider};
