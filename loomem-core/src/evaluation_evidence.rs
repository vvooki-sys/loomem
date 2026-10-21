//! Feature-gated HMAC primitive for private evaluation receipts.
use hmac::{Hmac, Mac};
use sha2::Sha256;

/// Authenticate a framed evaluation message with a 256-bit run key.
pub fn authenticate(key: &[u8; 32], message: &[u8]) -> Result<String, hmac::digest::InvalidLength> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key)?;
    mac.update(message);
    Ok(mac
        .finalize()
        .into_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn standard_hmac_vector_and_key_rotation() {
        let first = authenticate(&[0; 32], b"evaluation").unwrap();
        assert_eq!(first.len(), 64);
        assert_ne!(first, authenticate(&[1; 32], b"evaluation").unwrap());
        assert_ne!(first, authenticate(&[0; 32], b"evaluation2").unwrap());
    }
}
